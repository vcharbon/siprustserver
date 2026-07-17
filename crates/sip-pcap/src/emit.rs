//! Structured (JSON) emit of the flow model — the machine-readable twin of
//! the `sipflow` text presenter, for downstream extraction tooling that must
//! not scrape human-oriented output. Serialization only; the model itself
//! lives in [`crate::flow`].

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};
use sip_message::SipMessage;

use crate::flow::{CallGroup, FlowLeg, FlowMsg, Flows, MatchEvidence};
use crate::DecodeStats;

/// Value of the top-level `"schema"` field. Bumped on any breaking change to
/// the emitted shape; consumers reject versions they do not know.
pub const EMIT_SCHEMA_VERSION: u32 = 1;

/// Serialize the full flow model, plus the pcap decode counters, to JSON.
///
/// Emitted schema (version [`EMIT_SCHEMA_VERSION`]; stable contract for
/// downstream consumers). Socket addresses are `"ip:port"` strings (IPv6
/// bracketed); indices are 0-based; absent optionals are `null`:
///
/// ```text
/// {
///   "schema": 1,
///   "decode_stats": { records, non_ip, non_udp, snap_truncated, datagrams,
///                     fragments, reassembled, frag_dropped, tail_truncated },
///   "flow_stats":   { sip_messages, capture_dups, parse_failed, non_sip },
///   "legs": [ {                     // index = leg id referenced by groups
///     "call_id": str,
///     "hops": [ { "a": addr, "b": addr } ],   // ordered by first observation
///     "invite": { "ruri", "from_uri", "to_uri", "cseq" } | null,
///     "final_status": u16 | null,
///     "saw_180": bool,
///     "terminated_by": "BYE" | "CANCEL" | null,
///     "tokens": [ str ],
///     "msgs": [ {                   // capture-time order across all hops
///       "ts_us": u64,
///       "src": addr, "dst": addr,
///       "hop": idx,                 // into "hops" — filter on it for the
///                                   // per-hop stream (one vantage per leg)
///       "retx": bool,
///       "raw_b64": str,             // exact wire bytes (header order and
///                                   // casing preserved), standard base64
///       "summary": {
///         "kind": "request" | "response",
///         "method": str, "uri": str,          // request only
///         "status": u16, "reason": str,       // response only
///         "cseq": { "seq": u32, "method": str },
///         "from": { "uri": str, "tag": str|null },
///         "to":   { "uri": str, "tag": str|null }
///       }
///     } ]
///   } ],
///   "groups": [ {                   // ordered by first activity;
///     "legs": [ idx ],              // every leg is in exactly one group
///     "evidence": [                 // why members were joined (empty for a
///                                   // single-leg group) — heuristic, meant
///                                   // for human confirm/override downstream
///       { "kind": "shared_token", "token": str, "legs": [idx] }
///       | { "kind": "fromto_adjacency", "legs": [idx, idx],
///           "shared_host": ip, "dt_us": u64 }
///     ]
///   } ]
/// }
/// ```
pub fn flows_to_json(flows: &Flows, decode: &DecodeStats) -> Value {
    json!({
        "schema": EMIT_SCHEMA_VERSION,
        "decode_stats": {
            "records": decode.records,
            "non_ip": decode.non_ip,
            "non_udp": decode.non_udp,
            "snap_truncated": decode.snap_truncated,
            "datagrams": decode.datagrams,
            "fragments": decode.fragments,
            "reassembled": decode.reassembled,
            "frag_dropped": decode.frag_dropped,
            "tail_truncated": decode.tail_truncated,
        },
        "flow_stats": {
            "sip_messages": flows.stats.sip_messages,
            "capture_dups": flows.stats.capture_dups,
            "parse_failed": flows.stats.parse_failed,
            "non_sip": flows.stats.non_sip,
        },
        "legs": flows.legs.iter().map(leg_json).collect::<Vec<_>>(),
        "groups": flows.groups.iter().map(group_json).collect::<Vec<_>>(),
    })
}

fn leg_json(leg: &FlowLeg) -> Value {
    json!({
        "call_id": leg.call_id,
        "hops": leg
            .hops
            .iter()
            .map(|h| json!({ "a": h.a.to_string(), "b": h.b.to_string() }))
            .collect::<Vec<_>>(),
        "invite": leg.invite.as_ref().map(|inv| json!({
            "ruri": inv.ruri,
            "from_uri": inv.from_uri,
            "to_uri": inv.to_uri,
            "cseq": inv.cseq,
        })),
        "final_status": leg.final_status,
        "saw_180": leg.saw_180,
        "terminated_by": leg.terminated_by.map(|t| t.as_str()),
        "tokens": leg.tokens.iter().collect::<Vec<_>>(),
        "msgs": leg.msgs.iter().map(msg_json).collect::<Vec<_>>(),
    })
}

fn msg_json(m: &FlowMsg) -> Value {
    json!({
        "ts_us": m.ts_us,
        "src": m.src.to_string(),
        "dst": m.dst.to_string(),
        "hop": m.hop,
        "retx": m.retx,
        "raw_b64": BASE64.encode(m.raw()),
        "summary": summary_json(&m.parsed),
    })
}

fn summary_json(msg: &SipMessage) -> Value {
    match msg {
        SipMessage::Request(r) => json!({
            "kind": "request",
            "method": r.method.as_str(),
            "uri": r.uri,
            "cseq": { "seq": r.cseq.seq, "method": r.cseq.method.as_str() },
            "from": { "uri": r.from.uri, "tag": r.from.tag },
            "to": { "uri": r.to.uri, "tag": r.to.tag },
        }),
        SipMessage::Response(r) => json!({
            "kind": "response",
            "status": r.status,
            "reason": r.reason,
            "cseq": { "seq": r.cseq.seq, "method": r.cseq.method.as_str() },
            "from": { "uri": r.from.uri, "tag": r.from.tag },
            "to": { "uri": r.to.uri, "tag": r.to.tag },
        }),
    }
}

fn group_json(group: &CallGroup) -> Value {
    json!({
        "legs": group.legs,
        "evidence": group.evidence.iter().map(evidence_json).collect::<Vec<_>>(),
    })
}

fn evidence_json(ev: &MatchEvidence) -> Value {
    match ev {
        MatchEvidence::SharedToken { token, legs } => json!({
            "kind": "shared_token",
            "token": token,
            "legs": legs,
        }),
        MatchEvidence::FromToAdjacency { legs, shared_host, dt_us } => json!({
            "kind": "fromto_adjacency",
            "legs": legs,
            "shared_host": shared_host.to_string(),
            "dt_us": dt_us,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::{build_flows, FlowConfig};
    use crate::Datagram;

    fn dg(ts_us: u64, src: &str, dst: &str, payload: &[u8]) -> Datagram {
        Datagram {
            ts_us,
            src: src.parse().unwrap(),
            dst: dst.parse().unwrap(),
            payload: payload.to_vec(),
        }
    }

    fn sip_request(method: &str, call_id: &str, cseq: u32, branch: &str, extra: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@10.0.0.9 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=f1\r\n\
             To: <sip:bob@10.0.0.9>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn sip_response(status: u16, reason: &str, call_id: &str, cseq: u32, branch: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} {reason}\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             From: <sip:alice@10.0.0.1>;tag=f1\r\n\
             To: <sip:bob@10.0.0.9>;tag=t1\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// The JSON carries the whole model: stats, legs with hops/summaries,
    /// groups — and counts agree with the model it was built from.
    #[test]
    fn emits_full_model_with_matching_counts() {
        let inv = sip_request("INVITE", "emit-1", 1, "b1", "");
        let ok = sip_response(200, "OK", "emit-1", 1, "b1");
        let datagrams = vec![
            dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv),
            dg(2_000, "10.0.0.2:5062", "10.0.0.9:5060", &inv),
            dg(9_000, "10.0.0.9:5060", "10.0.0.2:5062", &ok),
            dg(10_000, "10.0.0.2:5060", "10.0.0.1:5060", &ok),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let decode = DecodeStats { records: 4, datagrams: 4, ..Default::default() };
        let v = flows_to_json(&flows, &decode);

        assert_eq!(v["schema"], EMIT_SCHEMA_VERSION);
        assert_eq!(v["decode_stats"]["records"], 4);
        assert_eq!(v["flow_stats"]["sip_messages"], flows.stats.sip_messages);
        assert_eq!(v["legs"].as_array().unwrap().len(), flows.legs.len());
        assert_eq!(v["groups"].as_array().unwrap().len(), flows.groups.len());
        let leg = &v["legs"][0];
        assert_eq!(leg["call_id"], "emit-1");
        assert_eq!(leg["hops"].as_array().unwrap().len(), flows.legs[0].hops.len());
        assert_eq!(leg["msgs"].as_array().unwrap().len(), flows.legs[0].msgs.len());
        assert_eq!(leg["invite"]["ruri"], "sip:bob@10.0.0.9");
        assert_eq!(leg["final_status"], 200);
        assert_eq!(leg["terminated_by"], Value::Null);
        // Per-hop stream: each message's hop index resolves into the hop chain.
        for m in leg["msgs"].as_array().unwrap() {
            let hop = m["hop"].as_u64().unwrap() as usize;
            assert!(hop < leg["hops"].as_array().unwrap().len());
        }
        // Request and response summaries carry their kind-specific fields.
        assert_eq!(leg["msgs"][0]["summary"]["kind"], "request");
        assert_eq!(leg["msgs"][0]["summary"]["method"], "INVITE");
        assert_eq!(leg["msgs"][0]["summary"]["from"]["tag"], "f1");
        assert_eq!(leg["msgs"][0]["summary"]["to"]["tag"], Value::Null);
        assert_eq!(leg["msgs"][2]["summary"]["kind"], "response");
        assert_eq!(leg["msgs"][2]["summary"]["status"], 200);
        assert_eq!(leg["msgs"][2]["summary"]["cseq"]["method"], "INVITE");
    }

    /// `raw_b64` round-trips to the exact wire bytes.
    #[test]
    fn raw_payload_base64_roundtrips() {
        let inv = sip_request("INVITE", "emit-raw", 1, "b1", "X-Mixed-Case-HDR: kept\r\n");
        let datagrams = vec![dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv)];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let v = flows_to_json(&flows, &DecodeStats::default());
        let b64 = v["legs"][0]["msgs"][0]["raw_b64"].as_str().unwrap();
        assert_eq!(BASE64.decode(b64).unwrap(), inv);
    }

    /// Both evidence variants serialize with their discriminating `kind`.
    #[test]
    fn match_evidence_variants_are_emitted() {
        let tok_a = sip_request("INVITE", "ev-a", 1, "ba", "X-Api-Call: call-9\r\n");
        let tok_b = sip_request("INVITE", "ev-b", 1, "bb", "X-Api-Call: call-9\r\n");
        let adj_a = sip_request("INVITE", "ev-c", 1, "bc", "");
        let adj_b = sip_request("INVITE", "ev-d", 1, "bd", "");
        let datagrams = vec![
            dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &tok_a),
            dg(2_000, "10.0.0.2:5062", "10.0.0.9:5060", &tok_b),
            dg(3_000, "10.0.1.1:5060", "10.0.1.5:5060", &adj_a),
            dg(4_000, "10.0.1.5:5062", "10.0.1.9:5060", &adj_b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let v = flows_to_json(&flows, &DecodeStats::default());
        assert_eq!(v["groups"].as_array().unwrap().len(), 2);
        let tok_ev = &v["groups"][0]["evidence"][0];
        assert_eq!(tok_ev["kind"], "shared_token");
        assert_eq!(tok_ev["token"], "call-9");
        assert_eq!(tok_ev["legs"], json!([0, 1]));
        let adj_ev = &v["groups"][1]["evidence"][0];
        assert_eq!(adj_ev["kind"], "fromto_adjacency");
        assert_eq!(adj_ev["legs"], json!([2, 3]));
        assert_eq!(adj_ev["shared_host"], "10.0.1.5");
        assert_eq!(adj_ev["dt_us"], 1_000);
    }

    /// Serialized output parses back (serde round-trip) — the `--json` CLI
    /// contract downstream tooling pipes into.
    #[test]
    fn string_form_parses_back() {
        let inv = sip_request("INVITE", "emit-rt", 1, "b1", "");
        let datagrams = vec![dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv)];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let s = flows_to_json(&flows, &DecodeStats::default()).to_string();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["legs"][0]["call_id"], "emit-rt");
    }
}
