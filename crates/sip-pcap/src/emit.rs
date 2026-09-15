//! Build the emitted document from the flow model — the machine-readable twin
//! of the `sipflow` text presenter, for downstream extraction tooling that must
//! not scrape human-oriented output.
//!
//! Projection only. The document's shape is [`crate::doc`], its derived fields
//! are [`crate::enrich`], and the model itself is [`crate::flow`].

use sip_message::SipMessage;

use crate::doc::{
    AlignedProbeJson, CSeqJson, DecodeStatsJson, Evidence, FlowStatsJson, FlowsDoc, GroupJson,
    HopJson, InviteJson, LegJson, MsgJson, Party, Payload, Summary,
};
use crate::enrich::{enrich, EnrichOptions};
use crate::flow::{CallGroup, FlowLeg, FlowMsg, Flows, MatchEvidence};
use crate::DecodeStats;

pub use crate::doc::EMIT_SCHEMA_VERSION;

/// Serialize the full flow model, plus the pcap decode counters, as an
/// enriched document.
pub fn flows_to_doc(
    flows: &Flows,
    decode: &DecodeStats,
    opts: &EnrichOptions,
) -> Result<FlowsDoc, String> {
    let all: Vec<usize> = (0..flows.groups.len()).collect();
    flows_to_doc_selected(flows, decode, &all, opts)
}

/// The same document restricted to `groups` — the extraction phase, where a
/// query has already decided which calls are wanted and only those should
/// carry their raw payloads.
///
/// Legs are renumbered to the emitted set so the document keeps its
/// invariants: `groups[*].legs` and every evidence leg reference index into
/// the emitted `legs`, and each emitted leg belongs to exactly one group.
/// Decode and flow counters describe the WHOLE capture regardless — they
/// report what was read, not what was selected.
pub fn flows_to_doc_selected(
    flows: &Flows,
    decode: &DecodeStats,
    groups: &[usize],
    opts: &EnrichOptions,
) -> Result<FlowsDoc, String> {
    let mut keep: Vec<usize> =
        groups.iter().filter_map(|&g| flows.groups.get(g)).flat_map(|g| g.legs.clone()).collect();
    keep.sort_unstable();
    keep.dedup();
    let remap = |old: usize| keep.binary_search(&old).unwrap_or(usize::MAX);

    let mut doc = FlowsDoc {
        schema: EMIT_SCHEMA_VERSION,
        emit_headers: Vec::new(),
        decode_stats: DecodeStatsJson {
            records: decode.records,
            non_ip: decode.non_ip,
            non_udp: decode.non_udp,
            snap_truncated: decode.snap_truncated,
            datagrams: decode.datagrams,
            fragments: decode.fragments,
            reassembled: decode.reassembled,
            frag_dropped: decode.frag_dropped,
            tail_truncated: decode.tail_truncated,
        },
        flow_stats: FlowStatsJson {
            sip_messages: flows.stats.sip_messages,
            capture_dups: flows.stats.capture_dups,
            parse_failed: flows.stats.parse_failed,
            non_sip: flows.stats.non_sip,
            aligned_probes: flows
                .stats
                .aligned_probes
                .iter()
                .map(|a| AlignedProbeJson {
                    probe: a.probe,
                    reference: a.reference,
                    from_us: a.from_us,
                    offset_us: a.offset_us,
                    pairs: a.pairs as u64,
                })
                .collect(),
        },
        legs: keep.iter().map(|&l| leg_json(&flows.legs[l])).collect(),
        groups: groups
            .iter()
            .filter_map(|&g| flows.groups.get(g))
            .map(|g| group_json(g, &remap))
            .collect(),
    };
    enrich(&mut doc, opts)?;
    Ok(doc)
}

fn leg_json(leg: &FlowLeg) -> LegJson {
    LegJson {
        call_id: leg.call_id.clone(),
        hops: leg.hops.iter().map(|h| HopJson { a: h.a.to_string(), b: h.b.to_string() }).collect(),
        invite: leg.invite.as_ref().map(|inv| InviteJson {
            ruri: inv.ruri.text().into_owned(),
            from_uri: inv.from_uri.text().into_owned(),
            to_uri: inv.to_uri.text().into_owned(),
            cseq: inv.cseq,
        }),
        final_status: leg.final_status,
        saw_180: leg.saw_180,
        terminated_by: leg.terminated_by.map(|t| t.as_str().to_string()),
        tokens: leg.tokens().iter().map(|t| (*t).to_string()).collect(),
        msgs: leg.msgs.iter().map(msg_json).collect(),
    }
}

fn msg_json(m: &FlowMsg) -> MsgJson {
    let body = match &m.parsed {
        SipMessage::Request(r) => r.body().clone(),
        SipMessage::Response(r) => r.body().clone(),
    };
    let mut json = MsgJson::new(
        m.ts_us,
        m.src.to_string(),
        m.dst.to_string(),
        m.hop,
        Payload::of(m.raw(), &body),
        summary_json(&m.parsed),
    );
    json.probe = m.probe;
    json
}

fn summary_json(msg: &SipMessage) -> Summary {
    let (from, to, cseq) = (msg.from(), msg.to(), msg.cseq());
    let from = Party { uri: from.uri().text().into_owned(), tag: from.tag().map(str::to_string) };
    let to = Party { uri: to.uri().text().into_owned(), tag: to.tag().map(str::to_string) };
    let cseq = CSeqJson { seq: cseq.seq(), method: cseq.method().as_str().to_string() };
    match msg {
        SipMessage::Request(r) => Summary::Request {
            method: r.method().as_str().to_string(),
            uri: r.request_uri().text().into_owned(),
            cseq,
            from,
            to,
        },
        SipMessage::Response(r) => {
            Summary::Response { status: r.status(), reason: r.reason().to_string(), cseq, from, to }
        }
    }
}

fn group_json(group: &CallGroup, remap: &impl Fn(usize) -> usize) -> GroupJson {
    GroupJson {
        legs: group.legs.iter().map(|&l| remap(l)).collect(),
        evidence: group.evidence.iter().map(|e| evidence_json(e, remap)).collect(),
        t0_us: 0,
        initial_invite: None,
        final_us: None,
        final_status: None,
        methods: Default::default(),
    }
}

fn evidence_json(ev: &MatchEvidence, remap: &impl Fn(usize) -> usize) -> Evidence {
    let legs_of = |ls: &[usize]| ls.iter().map(|&l| remap(l)).collect::<Vec<_>>();
    match ev {
        MatchEvidence::SharedToken { strategy, token, legs } => {
            Evidence::SharedToken { strategy: *strategy, token: token.clone(), legs: legs_of(legs) }
        }
        MatchEvidence::SharedHeaderParam { strategy, header, param, token, legs } => {
            Evidence::SharedHeaderParam {
                strategy: *strategy,
                header: header.clone(),
                param: param.clone(),
                token: token.clone(),
                legs: legs_of(legs),
            }
        }
        MatchEvidence::DerivedCallId {
            strategy,
            legs,
            prefix,
            as_socket,
            peer_socket,
            shared_hop,
            dt_us,
        } => Evidence::DerivedCallId {
            strategy: *strategy,
            legs: legs_of(legs),
            prefix: prefix.clone(),
            as_socket: as_socket.to_string(),
            peer_socket: peer_socket.to_string(),
            shared_hop: *shared_hop,
            dt_us: *dt_us,
        },
        MatchEvidence::IdentityAdjacency { strategy, legs, shared_host, dt_us } => {
            Evidence::IdentityAdjacency {
                strategy: *strategy,
                legs: legs_of(legs),
                shared_host: shared_host.to_string(),
                dt_us: *dt_us,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::{build_flows, FlowConfig};
    use crate::Datagram;
    use serde_json::Value;

    fn dg(ts_us: u64, src: &str, dst: &str, payload: &[u8]) -> Datagram {
        Datagram {
            ts_us,
            src: src.parse().unwrap(),
            dst: dst.parse().unwrap(),
            payload: payload.to_vec(),
            probe: 0,
        }
    }

    fn json(flows: &Flows, decode: &DecodeStats) -> Value {
        let doc = flows_to_doc(flows, decode, &EnrichOptions::default()).expect("model enriches");
        serde_json::to_value(&doc).expect("document serializes")
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
        let v = json(&flows, &decode);

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

    /// A fully-UTF-8 payload is emitted as plain text (`raw`), byte-exact
    /// through JSON string escaping, with no base64 form present.
    #[test]
    fn utf8_payload_emits_as_text() {
        let inv = sip_request("INVITE", "emit-raw", 1, "b1", "X-Mixed-Case-HDR: kept\r\n");
        let datagrams = vec![dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv)];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let v = json(&flows, &DecodeStats::default());
        let m = &v["legs"][0]["msgs"][0];
        assert_eq!(m["raw"].as_str().unwrap().as_bytes(), &inv[..]);
        assert!(m.get("raw_b64").is_none());
        assert!(m.get("head").is_none());
        // JSON round-trip preserves the exact bytes.
        let s = serde_json::to_string_pretty(&v).unwrap();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["legs"][0]["msgs"][0]["raw"].as_str().unwrap().as_bytes(), &inv[..]);
    }

    /// A binary body splits into a UTF-8 `head` and a base64 `body_b64`;
    /// reassembly (head ++ body) reproduces the exact wire bytes.
    #[test]
    fn binary_body_splits_into_head_and_body_b64() {
        let body: Vec<u8> = vec![0x30, 0x82, 0xff, 0x00, 0x9c, 0x01]; // ASN.1-ish
        let mut inv = format!(
            "INVITE sip:bob@10.0.0.9 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKmsd\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=f1\r\n\
             To: <sip:bob@10.0.0.9>\r\n\
             Call-ID: emit-msd\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Type: application/octet-stream\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        let head_len = inv.len();
        inv.extend_from_slice(&body);
        let datagrams = vec![dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv)];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.stats.sip_messages, 1, "binary-body INVITE must parse");
        let v = json(&flows, &DecodeStats::default());
        let m = &v["legs"][0]["msgs"][0];
        let head = m["head"].as_str().unwrap();
        assert_eq!(head.as_bytes(), &inv[..head_len]);
        assert!(head.ends_with("\r\n\r\n"));
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(m["body_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, body);
        let mut reassembled = head.as_bytes().to_vec();
        reassembled.extend_from_slice(&decoded);
        assert_eq!(reassembled, inv);
        assert!(m.get("raw").is_none());
        assert!(m.get("raw_b64").is_none());
    }

    /// All three evidence variants serialize with their discriminating
    /// `kind` and the pipeline strategy index that fired.
    #[test]
    fn match_evidence_variants_are_emitted() {
        let tok_a = sip_request("INVITE", "ev-a", 1, "ba", "X-Api-Call: call-9\r\n");
        let tok_b = sip_request("INVITE", "ev-b", 1, "bb", "X-Api-Call: call-9\r\n");
        let icid_a = sip_request(
            "INVITE",
            "ev-e",
            1,
            "be",
            "P-Charging-Vector: icid-value=icid-7;orig-ioi=a\r\n",
        );
        let icid_b = sip_request(
            "INVITE",
            "ev-f",
            1,
            "bf",
            "P-Charging-Vector: orig-ioi=b;icid-value=icid-7\r\n",
        );
        let der_a = sip_request("INVITE", "ev-derived-base", 1, "bg", "");
        let der_b = sip_request("INVITE", "1-ev-derived-base", 1, "bh", "");
        let adj_a = sip_request("INVITE", "ev-c", 1, "bc", "");
        let adj_b = sip_request("INVITE", "ev-d", 1, "bd", "");
        let datagrams = vec![
            dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &tok_a),
            dg(2_000, "10.0.0.2:5062", "10.0.0.9:5060", &tok_b),
            dg(3_000, "10.0.2.1:5060", "10.0.2.5:5060", &icid_a),
            dg(4_000, "10.0.2.5:5062", "10.0.2.9:5060", &icid_b),
            // The application-server loopback: the derived INVITE goes back
            // out the socket pair the base INVITE arrived on.
            dg(5_000, "10.0.3.1:5060", "10.0.3.5:5060", &der_a),
            dg(6_000, "10.0.3.5:5060", "10.0.3.1:5060", &der_b),
            dg(7_000, "10.0.1.1:5060", "10.0.1.5:5060", &adj_a),
            dg(8_000, "10.0.1.5:5062", "10.0.1.9:5060", &adj_b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let v = json(&flows, &DecodeStats::default());
        assert_eq!(v["groups"].as_array().unwrap().len(), 4);
        let tok_ev = &v["groups"][0]["evidence"][0];
        assert_eq!(tok_ev["kind"], "shared_token");
        assert_eq!(tok_ev["strategy"], 0);
        assert_eq!(tok_ev["token"], "call-9");
        assert_eq!(tok_ev["legs"], serde_json::json!([0, 1]));
        let icid_ev = &v["groups"][1]["evidence"][0];
        assert_eq!(icid_ev["kind"], "shared_header_param");
        assert_eq!(icid_ev["strategy"], 1);
        assert_eq!(icid_ev["header"], "P-Charging-Vector");
        assert_eq!(icid_ev["param"], "icid-value");
        assert_eq!(icid_ev["token"], "icid-7");
        assert_eq!(icid_ev["legs"], serde_json::json!([2, 3]));
        let der_ev = &v["groups"][2]["evidence"][0];
        assert_eq!(der_ev["kind"], "derived_call_id");
        assert_eq!(der_ev["strategy"], 2);
        assert_eq!(der_ev["legs"], serde_json::json!([4, 5]));
        assert_eq!(der_ev["prefix"], "1-");
        assert_eq!(der_ev["as_socket"], "10.0.3.5:5060");
        assert_eq!(der_ev["peer_socket"], "10.0.3.1:5060");
        assert_eq!(der_ev["shared_hop"], true);
        assert_eq!(der_ev["dt_us"], 1_000);
        let adj_ev = &v["groups"][3]["evidence"][0];
        assert_eq!(adj_ev["kind"], "identity_adjacency");
        assert_eq!(adj_ev["strategy"], 3);
        assert_eq!(adj_ev["legs"], serde_json::json!([6, 7]));
        assert_eq!(adj_ev["shared_host"], "10.0.1.5");
        assert_eq!(adj_ev["dt_us"], 1_000);
    }

    /// Serialized output parses back into the typed document — the `--json`
    /// CLI contract downstream tooling reads.
    #[test]
    fn string_form_parses_back() {
        let inv = sip_request("INVITE", "emit-rt", 1, "b1", "");
        let datagrams = vec![dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv)];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let s = serde_json::to_string(
            &flows_to_doc(&flows, &DecodeStats::default(), &EnrichOptions::default()).unwrap(),
        )
        .unwrap();
        let back: FlowsDoc = serde_json::from_str(&s).unwrap();
        assert_eq!(back.legs[0].call_id, "emit-rt");
    }
}
