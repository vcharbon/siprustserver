//! `source.txt` renderer — the complete, deterministic, agent-readable
//! callflow. Agents read THIS instead of the pcap, so completeness beats
//! prettiness and the output is
//! a golden-tested pure function of (capture name, model, proposals).

use std::fmt::Write as _;

use crate::model::{Evidence, Flows, Summary};
use crate::propose::Proposals;

/// Render the whole capture. Deterministic: iteration is index-ordered, all
/// times are integer-microsecond offsets from the capture's first message.
pub fn render(capture: &str, flows: &Flows, proposals: &Proposals) -> String {
    let mut out = String::new();
    let t0 = flows.legs.iter().flat_map(|l| l.msgs.first()).map(|m| m.ts_us).min().unwrap_or(0);

    let w = &mut out;
    let _ = writeln!(w, "pcap2test source v1");
    let _ = writeln!(w, "capture: {capture}");
    let _ = writeln!(w, "agents read this file instead of the pcap");
    let _ = writeln!(w);
    let d = &flows.decode_stats;
    let _ = writeln!(
        w,
        "decode_stats: records={} non_ip={} non_udp={} snap_truncated={} datagrams={} \
         fragments={} reassembled={} frag_dropped={} tail_truncated={}",
        d.records,
        d.non_ip,
        d.non_udp,
        d.snap_truncated,
        d.datagrams,
        d.fragments,
        d.reassembled,
        d.frag_dropped,
        d.tail_truncated
    );
    let f = &flows.flow_stats;
    let _ = writeln!(
        w,
        "flow_stats: sip_messages={} capture_dups={} parse_failed={} non_sip={}",
        f.sip_messages, f.capture_dups, f.parse_failed, f.non_sip
    );
    if f.header_failed > 0 {
        let _ = writeln!(w, "flow_stats: header_failed={}", f.header_failed);
    }
    for a in &f.aligned_probes {
        let _ = writeln!(
            w,
            "aligned_probe: probe={} reference={} from_us={} offset_ms={:.3} pairs={}",
            a.probe,
            a.reference,
            a.from_us,
            a.offset_us as f64 / 1000.0,
            a.pairs
        );
    }
    let _ = writeln!(
        w,
        "legs={} groups={} pairing_proposals={}",
        flows.legs.len(),
        flows.groups.len(),
        proposals.pairings.len()
    );
    let _ = writeln!(w, "t0_us={t0} (ladder times are +seconds from t0, capture-global)");

    for (gi, group) in flows.groups.iter().enumerate() {
        let _ = writeln!(w);
        let _ = writeln!(w, "== group {gi}: legs {:?}", group.legs);
        if group.evidence.is_empty() {
            let _ = writeln!(w, "evidence: none (single-leg group)");
        }
        for ev in &group.evidence {
            match ev {
                Evidence::SharedToken { strategy, token, legs } => {
                    let _ = writeln!(
                        w,
                        "evidence: shared_token[{strategy}] token={token:?} legs={legs:?}"
                    );
                }
                Evidence::SharedHeaderParam { strategy, header, param, token, legs } => {
                    let _ = writeln!(
                        w,
                        "evidence: shared_header_param[{strategy}] {header};{param}={token:?} legs={legs:?}"
                    );
                }
                Evidence::DerivedCallId {
                    strategy,
                    legs,
                    prefix,
                    as_socket,
                    peer_socket,
                    shared_hop,
                    dt_us,
                } => {
                    let _ = writeln!(
                        w,
                        "evidence: derived_call_id[{strategy}] legs={legs:?} prefix={prefix:?} \
                         as={as_socket} peer={peer_socket} shared_hop={shared_hop} dt_us={dt_us}"
                    );
                }
                Evidence::IdentityAdjacency { strategy, legs, shared_host, dt_us } => {
                    let _ = writeln!(
                        w,
                        "evidence: identity_adjacency[{strategy}] legs={legs:?} shared_host={shared_host} dt_us={dt_us}"
                    );
                }
            }
        }
        for &li in &group.legs {
            render_leg(w, flows, proposals, li, t0);
        }
    }

    let _ = writeln!(w);
    let _ = writeln!(w, "== pairing proposals (heuristic — never asserted)");
    if proposals.pairings.is_empty() {
        let _ = writeln!(w, "none");
    }
    for p in &proposals.pairings {
        let _ = writeln!(
            w,
            "pairing legs={}+{}: leg {} call_id = {:?} + leg {} call_id",
            p.base, p.derived, p.derived, p.prefix, p.base
        );
        for sh in &p.shared_hops {
            let _ = writeln!(
                w,
                "  shared hop: {} <-> {} (leg {} hop {} / leg {} hop {})",
                sh.a, sh.b, p.base, sh.base_hop, p.derived, sh.derived_hop
            );
        }
        let _ = writeln!(w, "  first-msg dt_us={}", p.first_msg_dt_us);
    }
    out
}

fn render_leg(w: &mut String, flows: &Flows, proposals: &Proposals, li: usize, t0: u64) {
    let leg = &flows.legs[li];
    let _ = writeln!(w);
    let _ = writeln!(w, "-- leg {li}: call_id={}", leg.call_id);
    match &leg.invite {
        Some(inv) => {
            let _ = writeln!(
                w,
                "invite: ruri={} from={} to={} cseq={}",
                inv.ruri, inv.from_uri, inv.to_uri, inv.cseq
            );
        }
        None => {
            let _ = writeln!(w, "invite: none");
        }
    }
    let _ = writeln!(
        w,
        "final_status={} saw_180={} terminated_by={} tokens={}",
        opt(leg.final_status.map(|s| s.to_string())),
        if leg.saw_180 { "yes" } else { "no" },
        opt(leg.terminated_by.clone()),
        if leg.tokens.is_empty() { "none".to_string() } else { leg.tokens.join(",") }
    );
    let _ = writeln!(w, "hops (ordered by first observation; * = vantage proposal):");
    for (hi, hop) in leg.hops.iter().enumerate() {
        let mark = if proposals.vantage.get(li) == Some(&hi) { "*" } else { " " };
        let msgs = leg.msgs.iter().filter(|m| m.hop == hi).count();
        let _ = writeln!(w, "  hop {hi} {mark} {} <-> {} msgs={msgs}", hop.a, hop.b);
    }
    let _ = writeln!(
        w,
        "ladder ({} msgs; m<idx> +t h<hop> src -> dst | message | CSeq | from-tag/to-tag | bytes):",
        leg.msgs.len()
    );
    for (mi, msg) in leg.msgs.iter().enumerate() {
        let off = msg.ts_us - t0;
        let what = match &msg.summary {
            Summary::Request { method, uri, .. } => format!("{method} {uri}"),
            Summary::Response { status, reason, .. } => format!("{status} {reason}"),
        };
        let cseq = msg.summary.cseq();
        let _ = writeln!(
            w,
            "  m{mi} +{}.{:06} h{} {} -> {} | {what} | CSeq {} {} | {}/{} | {}B{}",
            off / 1_000_000,
            off % 1_000_000,
            msg.hop,
            msg.src,
            msg.dst,
            cseq.seq,
            cseq.method,
            opt(msg.summary.from().tag.clone()),
            opt(msg.summary.to().tag.clone()),
            crate::model::wire(msg).len(),
            if msg.retx { " RETX" } else { "" }
        );
    }
}

fn opt(v: Option<String>) -> String {
    v.unwrap_or_else(|| "-".to_string())
}
