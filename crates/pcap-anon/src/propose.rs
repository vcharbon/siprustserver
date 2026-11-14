//! Heuristic proposals derived from the flow model: a default observation
//! vantage per leg and cross-B2BUA leg-pairing candidates. Proposals are
//! NEVER facts, and `source.txt`
//! presents them as such. All outputs are deterministic functions of the model.

use crate::index::Derivations;
use crate::model::{Flows, Leg};

/// Everything downstream renderers need beyond the raw model.
#[derive(Debug)]
pub struct Proposals {
    /// Proposed observation hop per leg (index parallel to `flows.legs`).
    pub vantage: Vec<usize>,
    /// Cross-group pairing candidates, ordered by (base, derived) leg index.
    pub pairings: Vec<PairingProposal>,
}

/// A heuristic claim that two legs are the two sides of one B2BUA call.
///
/// Candidate generation is call-id derivation only: `derived`'s Call-ID is
/// `prefix` + `base`'s Call-ID (the `term1-` / `1-` relay convention).
/// Shared hops and the first-message time delta are corroborating evidence
/// attached for the human, never generation criteria on their own.
#[derive(Debug)]
pub struct PairingProposal {
    pub base: usize,
    pub derived: usize,
    /// The extra Call-ID prefix on the derived leg (e.g. `"term1-"`).
    pub prefix: String,
    pub shared_hops: Vec<SharedHop>,
    /// |first message ts delta| between the two legs, microseconds.
    pub first_msg_dt_us: u64,
}

/// One socket pair observed in both legs' hop chains.
#[derive(Debug)]
pub struct SharedHop {
    pub a: String,
    pub b: String,
    pub base_hop: usize,
    pub derived_hop: usize,
}

/// Compute vantage and pairing proposals for a whole capture.
pub fn propose(flows: &Flows) -> Proposals {
    propose_indexed(flows, &Derivations::build(flows))
}

/// [`propose`] over an index the caller already built.
pub fn propose_indexed(flows: &Flows, index: &Derivations) -> Proposals {
    Proposals {
        vantage: flows.legs.iter().map(propose_vantage).collect(),
        pairings: propose_pairings(flows, index),
    }
}

/// Proposed vantage: the hop that observed the most of the leg's messages,
/// ties broken toward the first-observed hop (v1 heuristic).
fn propose_vantage(leg: &Leg) -> usize {
    let mut counts = vec![0usize; leg.hops.len()];
    for msg in &leg.msgs {
        counts[msg.hop] += 1;
    }
    counts
        .iter()
        .enumerate()
        .max_by(|(ia, na), (ib, nb)| na.cmp(nb).then(ib.cmp(ia)))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// One proposal per (base, derived) pair of legs where `derived`'s Call-ID is
/// `<nonempty prefix> + base`'s — the relayed-leg naming convention this
/// heuristic keys on — read off the suffix index, never by enumerating pairs.
fn propose_pairings(flows: &Flows, index: &Derivations) -> Vec<PairingProposal> {
    let mut out = Vec::new();
    for (derived, derived_leg) in flows.legs.iter().enumerate() {
        for &base in index.suffixes_of(derived) {
            if base == derived || index.same_group(base, derived) {
                continue; // already grouped upstream — its evidence stands on its own
            }
            let base_leg = &flows.legs[base];
            out.push(PairingProposal {
                base,
                derived,
                prefix: derived_leg.call_id[..derived_leg.call_id.len() - base_leg.call_id.len()]
                    .to_string(),
                shared_hops: shared_hops(base_leg, derived_leg),
                first_msg_dt_us: first_msg_dt_us(base_leg, derived_leg),
            });
        }
    }
    out.sort_by_key(|p| (p.base, p.derived));
    out
}

fn shared_hops(base: &Leg, derived: &Leg) -> Vec<SharedHop> {
    let mut out = Vec::new();
    for (bi, bh) in base.hops.iter().enumerate() {
        for (di, dh) in derived.hops.iter().enumerate() {
            if crate::model::same_pair(bh, dh) {
                out.push(SharedHop {
                    a: bh.a.clone(),
                    b: bh.b.clone(),
                    base_hop: bi,
                    derived_hop: di,
                });
            }
        }
    }
    out
}

fn first_msg_dt_us(a: &Leg, b: &Leg) -> u64 {
    match (a.msgs.first(), b.msgs.first()) {
        (Some(ma), Some(mb)) => ma.ts_us.abs_diff(mb.ts_us),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ingest_str;

    fn two_leg_fixture(base_cid: &str, derived_cid: &str, grouped: bool) -> Flows {
        let groups = if grouped {
            r#"[{"legs":[0,1],"evidence":[]}]"#.to_string()
        } else {
            r#"[{"legs":[0],"evidence":[]},{"legs":[1],"evidence":[]}]"#.to_string()
        };
        let leg = |cid: &str, ts: u64, hops: &str, hop_of_msgs: &[usize]| {
            let msgs: Vec<String> = hop_of_msgs
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    format!(
                        r#"{{"ts_us":{},"src":"10.0.0.1:5060","dst":"10.0.0.2:5060",
                            "hop":{h},"retx":false,"raw_b64":"T1BUSU9OUw==",
                            "summary":{{"kind":"request","method":"OPTIONS","uri":"sip:x",
                                       "cseq":{{"seq":1,"method":"OPTIONS"}},
                                       "from":{{"uri":"sip:a","tag":"t"}},
                                       "to":{{"uri":"sip:b","tag":null}}}}}}"#,
                        ts + i as u64
                    )
                })
                .collect();
            format!(
                r#"{{"call_id":"{cid}","hops":{hops},"invite":null,"final_status":null,
                    "saw_180":false,"terminated_by":null,"tokens":[],"msgs":[{}]}}"#,
                msgs.join(",")
            )
        };
        let hops_a = r#"[{"a":"10.0.0.1:5060","b":"10.0.0.2:5060"},{"a":"10.0.0.3:5060","b":"10.0.0.4:5060"}]"#;
        let hops_b = r#"[{"a":"10.0.0.4:5060","b":"10.0.0.3:5060"}]"#;
        let json = format!(
            r#"{{"schema":5,
                "decode_stats":{{"records":0,"non_ip":0,"non_udp":0,"snap_truncated":0,
                                 "datagrams":0,"fragments":0,"reassembled":0,"frag_dropped":0,
                                 "tail_truncated":0}},
                "flow_stats":{{"sip_messages":0,"capture_dups":0,"parse_failed":0,"non_sip":0}},
                "legs":[{},{}],
                "groups":{groups}}}"#,
            leg(base_cid, 1_000, hops_a, &[0, 1, 1]),
            leg(derived_cid, 1_250, hops_b, &[0]),
        );
        ingest_str(&json).unwrap()
    }

    #[test]
    fn callid_derivation_yields_a_proposal_with_evidence() {
        let flows = two_leg_fixture("abc@host", "term1-abc@host", false);
        let p = propose(&flows);
        assert_eq!(p.pairings.len(), 1);
        let pair = &p.pairings[0];
        assert_eq!((pair.base, pair.derived), (0, 1));
        assert_eq!(pair.prefix, "term1-");
        assert_eq!(pair.first_msg_dt_us, 250);
        // Shared hop matches regardless of a/b orientation.
        assert_eq!(pair.shared_hops.len(), 1);
        assert_eq!(pair.shared_hops[0].base_hop, 1);
        assert_eq!(pair.shared_hops[0].derived_hop, 0);
    }

    #[test]
    fn unrelated_callids_yield_no_proposal() {
        let flows = two_leg_fixture("abc@host", "def@host", false);
        assert!(propose(&flows).pairings.is_empty());
    }

    #[test]
    fn already_grouped_legs_are_not_re_proposed() {
        let flows = two_leg_fixture("abc@host", "term1-abc@host", true);
        assert!(propose(&flows).pairings.is_empty());
    }

    /// `pairs` base legs `b<i>@host`, each with a `1-`-prefixed derived leg in
    /// its own group; a few unpaired legs; a leg in two groups; a duplicate
    /// Call-ID row; and legs whose Call-ID is a short suffix of every base
    /// (they pair with all of them: the derivation test has no minimum base
    /// length).
    fn synthetic(pairs: usize) -> Flows {
        use crate::testflow::{flows, leg, peer_req, sut_resp};
        let ladder = |ms: u64| [peer_req(ms, "INVITE"), sut_resp(ms + 1, 200)];
        let mut legs = Vec::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for i in 0..pairs {
            let base = format!("b{i:06}@host");
            legs.push(leg(&base, &ladder(i as u64 * 10)));
            legs.push(leg(&format!("1-{base}"), &ladder(i as u64 * 10 + 2)));
            groups.push(vec![2 * i, 2 * i + 1]);
        }
        for i in 0..3 {
            legs.push(leg(&format!("alone{i}@elsewhere"), &ladder(1)));
            groups.push(vec![legs.len() - 1]);
        }
        for suffix in ["host", "@host"] {
            legs.push(leg(suffix, &ladder(2)));
            groups.push(vec![legs.len() - 1]);
        }
        // Leg 1 sits in a second group too, with a duplicate of leg 0's Call-ID.
        legs.push(leg("b000000@host", &ladder(3)));
        groups.push(vec![1, legs.len() - 1]);
        let groups: Vec<&[usize]> = groups.iter().map(Vec::as_slice).collect();
        flows(&legs, &groups)
    }

    /// The pairwise form the indexed pairing must reproduce exactly.
    fn reference_pairings(flows: &Flows) -> Vec<PairingProposal> {
        let same_group = |a: usize, b: usize| {
            flows.groups.iter().any(|g| g.legs.contains(&a) && g.legs.contains(&b))
        };
        let mut out = Vec::new();
        for (base, base_leg) in flows.legs.iter().enumerate() {
            for (derived, derived_leg) in flows.legs.iter().enumerate() {
                if base == derived || same_group(base, derived) {
                    continue;
                }
                let (b, d) = (&base_leg.call_id, &derived_leg.call_id);
                if !(d.len() > b.len() && d.ends_with(b.as_str())) {
                    continue;
                }
                out.push(PairingProposal {
                    base,
                    derived,
                    prefix: d[..d.len() - b.len()].to_string(),
                    shared_hops: shared_hops(base_leg, derived_leg),
                    first_msg_dt_us: first_msg_dt_us(base_leg, derived_leg),
                });
            }
        }
        out.sort_by_key(|p| (p.base, p.derived));
        out
    }

    #[test]
    #[ignore = "slow lane: real clock >= 1 s"]
    fn indexed_pairing_equals_the_pairwise_reference() {
        let flows = synthetic(300);
        let got = propose_pairings(&flows, &Derivations::build(&flows));
        let want = reference_pairings(&flows);
        assert!(want.len() > 2 * 300, "the short suffixes pair with every base");
        assert_eq!(format!("{got:?}"), format!("{want:?}"));
    }

    /// The candidates the index visits — one probe per proper suffix of every
    /// Call-ID, one visit per suffix pair found — double when the legs double:
    /// no all-pairs walk.
    #[test]
    fn pairing_candidates_grow_linearly_with_legs() {
        let work = |pairs: usize| {
            let flows = synthetic(pairs);
            let index = Derivations::build(&flows);
            let probes: usize = flows.legs.iter().map(|l| l.call_id.chars().count() - 1).sum();
            let visits: usize = (0..flows.legs.len()).map(|d| index.suffixes_of(d).len()).sum();
            let proposals = propose_pairings(&flows, &index).len();
            assert!(visits >= proposals);
            probes + visits
        };
        let (w1, w2) = (work(600), work(1200));
        assert!(w2 <= 2 * w1 + 200, "600 pairs: {w1} candidates, 1200 pairs: {w2}");
    }

    /// Doubling the legs at most triples the pairing time: a manual check, the
    /// box's parallel lanes make wall time no test.
    #[test]
    #[ignore]
    fn pairing_work_grows_linearly_with_legs() {
        use std::time::{Duration, Instant};
        let timed = |pairs: usize| {
            let flows = synthetic(pairs);
            let start = Instant::now();
            let n = propose_pairings(&flows, &Derivations::build(&flows)).len();
            (start.elapsed(), n)
        };
        let (t1, n1) = timed(600);
        let (t2, n2) = timed(1200);
        assert!(n2 > n1);
        let bound = t1.max(Duration::from_millis(20)) * 3;
        assert!(t2 < bound, "600 legs: {t1:?}, 1200 legs: {t2:?} (bound {bound:?})");
    }

    #[test]
    fn vantage_prefers_busiest_hop_then_first_observed() {
        let flows = two_leg_fixture("abc@host", "term1-abc@host", false);
        let p = propose(&flows);
        assert_eq!(p.vantage[0], 1); // 2 msgs on hop 1 vs 1 on hop 0
        assert_eq!(p.vantage[1], 0);
    }
}
