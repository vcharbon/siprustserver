use super::*;
use crate::callfacts::REPEAT_ENVELOPE_US;

fn dg(ts_us: u64, src: &str, dst: &str, payload: &[u8]) -> Datagram {
    Datagram {
        ts_us,
        src: src.parse().unwrap(),
        dst: dst.parse().unwrap(),
        payload: payload.to_vec(),
        probe: 0,
    }
}

/// Minimal RFC-shaped message; `extra` lines are inserted before the body.
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

/// A proxy chain preserves Call-ID: the same INVITE observed at three
/// vantages plus responses at two of them → 5 ordered (src,dst) pairs but
/// 3 hops, each message assigned to its vantage.
#[test]
fn one_leg_splits_by_observation_hop() {
    let inv = sip_request("INVITE", "hop-split-1", 1, "b1", "");
    let ok = sip_response(200, "OK", "hop-split-1", 1, "b1");
    let datagrams = vec![
        dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv),
        dg(2_000, "10.0.0.3:5071", "10.0.0.4:5060", &inv),
        dg(3_000, "10.0.0.5:5060", "10.0.0.9:5060", &inv),
        dg(9_000, "10.0.0.9:5060", "10.0.0.5:5060", &ok),
        dg(10_000, "10.0.0.2:5060", "10.0.0.1:5060", &ok),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.legs.len(), 1);
    let leg = &flows.legs[0];
    assert_eq!(leg.hops.len(), 3, "3 vantages from 5 ordered socket pairs");
    assert_eq!(leg.msgs.len(), 5);
    // Hop chain ordered by first observation.
    assert!(leg.hops[0].contains("10.0.0.1:5060".parse().unwrap()));
    assert!(leg.hops[1].contains("10.0.0.3:5071".parse().unwrap()));
    assert!(leg.hops[2].contains("10.0.0.9:5060".parse().unwrap()));
    // Request and its response normalize to the same hop.
    assert_eq!(leg.msgs[0].hop, 0);
    assert_eq!(leg.msgs[4].hop, 0);
    assert_eq!(leg.msgs[2].hop, 2);
    assert_eq!(leg.msgs[3].hop, 2);
    assert_eq!(leg.msgs_at(2).count(), 2);
    // Per-hop selection sees a complete INVITE/200 exchange at hop 0.
    let at0: Vec<_> = leg.msgs_at(0).collect();
    assert!(matches!(&at0[0].parsed, SipMessage::Request(r) if r.method() == Method::Invite));
    assert!(matches!(&at0[1].parsed, SipMessage::Response(r) if r.status() == 200));
}

/// Same (src, dst, payload) within the capture-dup window is the capture
/// stack (collapsed, counted); past the window it is a real
/// retransmission (kept, retx-tagged).
#[test]
fn capture_dup_collapsed_but_retransmission_kept() {
    let inv = sip_request("INVITE", "dup-1", 1, "b1", "");
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv),
        dg(1_000_050, "10.0.0.1:5060", "10.0.0.2:5060", &inv), // veth+bridge dup
        dg(1_600_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv), // Timer-T1 retx
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.stats.capture_dups, 1);
    assert_eq!(flows.stats.sip_messages, 2);
    let leg = &flows.legs[0];
    assert_eq!(leg.msgs.len(), 2);
    assert!(!leg.msgs[0].retx);
    assert!(leg.msgs[1].retx);
}

/// Timer B ends the transaction at 64·T1, so the same bytes emitted after
/// it are a fresh emission, not a retransmission.
#[test]
fn a_repeat_past_the_transaction_envelope_is_not_a_retransmission() {
    let inv = sip_request("INVITE", "late-1", 1, "b1", "");
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv),
        dg(1_000_000 + REPEAT_ENVELOPE_US, "10.0.0.1:5060", "10.0.0.2:5060", &inv),
        dg(1_000_000 + REPEAT_ENVELOPE_US + 1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv),
    ];
    let leg = &build_flows(&datagrams, &FlowConfig::default()).legs[0];
    assert_eq!(leg.msgs.len(), 3);
    assert_eq!(leg.msgs.iter().map(|m| m.retx).collect::<Vec<_>>(), vec![false, true, false]);
}

fn dgp(ts_us: u64, src: &str, dst: &str, payload: &[u8], probe: u32) -> Datagram {
    Datagram { probe, ..dg(ts_us, src, dst, payload) }
}

/// A response to a non-INVITE transaction (`sip_response` answers an INVITE).
fn sip_response_to(status: u16, call_id: &str, cseq: u32, method: &str, branch: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             From: <sip:alice@10.0.0.1>;tag=f1\r\n\
             To: <sip:bob@10.0.0.9>;tag=t1\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// One dialog as two probes with a 250 ms clock offset write it: every
/// datagram twice, past the dedup window. Probe 0 sits at the caller's end
/// (its copy of a request precedes probe 1's by offset + transit, its copy
/// of a response by offset - transit).
///
/// Returns the datagrams and the count of messages the wire carried.
fn doubled_dialog(offset_us: u64) -> (Vec<Datagram>, usize) {
    const A: &str = "10.0.0.1:5060";
    const B: &str = "10.0.0.2:5060";
    const TRANSIT: u64 = 7_000;
    let inv = sip_request("INVITE", "doubled-1", 1, "b1", "");
    let trying = sip_response(100, "Trying", "doubled-1", 1, "b1");
    let ringing = sip_response(180, "Ringing", "doubled-1", 1, "b1");
    let ok = sip_response(200, "OK", "doubled-1", 1, "b1");
    let ack = sip_request("ACK", "doubled-1", 1, "b1", "");
    let bye = sip_request("BYE", "doubled-1", 2, "b2", "");
    let ok_bye = sip_response_to(200, "doubled-1", 2, "BYE", "b2");
    // (ts at the caller's end, from caller?, payload)
    let wire: Vec<(u64, bool, &[u8])> = vec![
        (1_000_000, true, &inv),
        (1_010_000, false, &trying),
        (1_100_000, false, &ringing),
        (1_500_000, false, &ok),
        (2_000_000, false, &ok), // the UAS's own T1 repeat of its 2xx (RFC 3261 §13.3.1.4)
        (2_020_000, true, &ack), // the slow ACK that stops the ladder
        (3_000_000, true, &bye),
        (3_010_000, false, &ok_bye),
    ];
    let mut out = Vec::new();
    for (ts, from_caller, payload) in &wire {
        let (src, dst) = if *from_caller { (A, B) } else { (B, A) };
        let far = if *from_caller { ts + TRANSIT } else { ts - TRANSIT };
        out.push(dgp(*ts, src, dst, payload, 0));
        out.push(dgp(far + offset_us, src, dst, payload, 1));
    }
    (out, wire.len())
}

fn methods(leg: &FlowLeg) -> Vec<String> {
    leg.msgs
        .iter()
        .map(|m| match &m.parsed {
            SipMessage::Request(r) => r.method().to_string(),
            SipMessage::Response(r) => format!("{} {}", r.status(), r.cseq().method()),
        })
        .collect()
}

/// Two probes writing one dialog with a constant clock offset past the
/// dedup window are two vantages on one wire: the copies collapse, the
/// UAS's own T1 repeat of its 2xx stays a retransmission, and an ACK or a
/// 2xx to a non-INVITE — classes that never retransmit (RFC 3261 §17.1.1.3,
/// §17.2.2) — carry no repeat at all.
#[test]
fn a_probe_pair_with_a_constant_offset_collapses_to_one_vantage() {
    let (datagrams, messages) = doubled_dialog(250_000);
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.legs.len(), 1);
    let leg = &flows.legs[0];
    assert_eq!(
        methods(leg),
        vec![
            "INVITE",
            "100 INVITE",
            "180 INVITE",
            "200 INVITE",
            "200 INVITE",
            "ACK",
            "BYE",
            "200 BYE"
        ],
        "every datagram once, in wire order, the T1 repeat kept"
    );
    assert_eq!(leg.msgs.len(), messages);
    assert_eq!(
        leg.msgs.iter().map(|m| m.retx).collect::<Vec<_>>(),
        vec![false, false, false, false, true, false, false, false],
        "only the UAS's T1 repeat of its 2xx is a retransmission"
    );
    assert_eq!(flows.stats.capture_dups as usize, messages);
    assert_eq!(flows.stats.sip_messages as usize, messages);
    let [aligned] = flows.stats.aligned_probes.as_slice() else {
        panic!("one probe rebased onto the other: {:?}", flows.stats.aligned_probes)
    };
    assert_eq!((aligned.probe, aligned.reference, aligned.from_us), (1, 0, 0));
    assert!((243_000..=257_000).contains(&aligned.offset_us), "{aligned:?}");
    assert_eq!(aligned.pairs, messages, "every datagram both probes wrote, the T1 repeat included");
}

/// A message only the rebased probe saw lands on the reference clock, so
/// it orders against the messages the other probe saw.
#[test]
fn a_rebased_probes_own_messages_land_on_the_reference_clock() {
    let (mut datagrams, _) = doubled_dialog(250_000);
    // Probe 1 alone sees a 183 the wire carried 50 ms after the 180 — the
    // caller-end probe missed it. On probe 1's clock it is stamped
    // 1_150_000 - 7_000 + 250_000.
    let early = sip_response(183, "Session Progress", "doubled-1", 1, "b1");
    datagrams.push(dgp(1_393_000, "10.0.0.2:5060", "10.0.0.1:5060", &early, 1));
    let flows = build_flows(&datagrams, &FlowConfig::default());
    let leg = &flows.legs[0];
    let at = leg
        .msgs
        .iter()
        .find(|m| matches!(&m.parsed, SipMessage::Response(r) if r.status() == 183))
        .expect("the 183 is ingested");
    assert_eq!(at.probe, 1);
    assert!(
        (1_140_000..=1_150_000).contains(&at.ts_us),
        "rebased by the pair's offset, transit aside: {}",
        at.ts_us
    );
    let idx = leg.msgs.iter().position(|m| std::ptr::eq(m, at)).unwrap();
    assert_eq!(methods(leg)[idx - 1], "180 INVITE");
    assert_eq!(methods(leg)[idx + 1], "200 INVITE");
}

/// A probe pair whose offset the dedup window already absorbs is left on
/// its own clocks: nothing to align, nothing reported.
#[test]
fn a_probe_pair_inside_the_dedup_window_is_not_rebased() {
    let (datagrams, messages) = doubled_dialog(50_000);
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.legs[0].msgs.len(), messages);
    assert_eq!(flows.stats.capture_dups as usize, messages);
    assert!(flows.stats.aligned_probes.is_empty());
}

/// Fewer shared datagrams than an offset can be estimated from: the copies
/// survive, and no offset is stated.
#[test]
fn a_probe_pair_sharing_too_few_datagrams_is_not_aligned() {
    let inv = sip_request("INVITE", "few-1", 1, "b1", "");
    let ok = sip_response(200, "OK", "few-1", 1, "b1");
    let datagrams = vec![
        dgp(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv, 0),
        dgp(1_250_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv, 1),
        dgp(2_000_000, "10.0.0.2:5060", "10.0.0.1:5060", &ok, 0),
        dgp(2_250_000, "10.0.0.2:5060", "10.0.0.1:5060", &ok, 1),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.legs[0].msgs.len(), 4);
    assert!(flows.stats.aligned_probes.is_empty());
}

/// One probe's copies of a T1 ladder pair with the other's rung by rung, so
/// the offset is the clock's and never the ladder's, whatever the ladder's
/// gaps: the ladder survives in full on the reference clock.
#[test]
fn a_ladder_both_probes_wrote_survives_the_alignment() {
    let inv = sip_request("INVITE", "ladder-1", 1, "b1", "");
    let ok = sip_response(200, "OK", "ladder-1", 1, "b1");
    let ack = sip_request("ACK", "ladder-1", 1, "b1", "");
    let mut datagrams = Vec::new();
    for rung in [0u64, 500_000, 1_500_000, 3_500_000] {
        datagrams.push(dgp(1_000_000 + rung, "10.0.0.1:5060", "10.0.0.2:5060", &inv, 0));
        datagrams.push(dgp(1_000_000 + rung + 600_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv, 1));
    }
    datagrams.push(dgp(5_000_000, "10.0.0.2:5060", "10.0.0.1:5060", &ok, 0));
    datagrams.push(dgp(5_600_000, "10.0.0.2:5060", "10.0.0.1:5060", &ok, 1));
    datagrams.push(dgp(5_100_000, "10.0.0.1:5060", "10.0.0.2:5060", &ack, 0));
    datagrams.push(dgp(5_700_000, "10.0.0.1:5060", "10.0.0.2:5060", &ack, 1));
    let flows = build_flows(&datagrams, &FlowConfig::default());
    let leg = &flows.legs[0];
    assert_eq!(methods(leg), vec!["INVITE", "INVITE", "INVITE", "INVITE", "200 INVITE", "ACK"]);
    assert_eq!(
        leg.msgs.iter().map(|m| m.retx).collect::<Vec<_>>(),
        vec![false, true, true, true, false, false]
    );
    assert_eq!(
        leg.msgs.iter().map(|m| m.ts_us).collect::<Vec<_>>(),
        vec![1_000_000, 1_500_000, 2_500_000, 4_500_000, 5_000_000, 5_100_000]
    );
    assert_eq!(flows.stats.aligned_probes[0].offset_us, 600_000);
    assert_eq!(flows.stats.aligned_probes[0].pairs, 6);
}

/// One probe wrote the first emission of three INVITEs toward a host that
/// never answers, the other probe their T1 rungs alone: the deltas agree at
/// T1 to the millisecond, and nothing shared rides no timer. That is not a
/// clock offset — the ladders survive whole and no probe moves.
#[test]
fn a_ladder_split_between_two_probes_is_not_an_offset() {
    let mut datagrams = Vec::new();
    for (n, call_id) in ["split-1", "split-2", "split-3"].iter().enumerate() {
        let inv = sip_request("INVITE", call_id, 1, "b1", "");
        let t0 = 1_000_000 + n as u64 * 10_000_000;
        datagrams.push(dgp(t0, "10.0.0.1:5060", "10.0.0.2:5060", &inv, 1));
        for rung in [500_000u64, 1_500_000, 3_500_000] {
            datagrams.push(dgp(t0 + rung, "10.0.0.1:5060", "10.0.0.2:5060", &inv, 0));
        }
    }
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert!(flows.stats.aligned_probes.is_empty(), "{:?}", flows.stats.aligned_probes);
    assert_eq!(flows.stats.capture_dups, 0);
    for leg in &flows.legs {
        assert_eq!(
            leg.msgs.iter().map(|m| m.retx).collect::<Vec<_>>(),
            vec![false, true, true, true]
        );
    }
}

/// A probe's clock STEPS mid-capture: the pair agrees to the transit for
/// the first exchange, then reads 440 ms apart for the second. Each
/// stretch is aligned on its own: the early copies collapse as they are,
/// the late ones once rebased, and the offset is reported from the step.
#[test]
fn a_clock_step_is_aligned_stretch_by_stretch() {
    // Two dialogs on one hop, 3000 s apart; both probes write everything.
    let mut datagrams = Vec::new();
    for (n, offset) in [(0u64, 0u64), (1, 440_000)] {
        let call_id = format!("step-{n}");
        let inv = sip_request("INVITE", &call_id, 1, "b1", "");
        let ok = sip_response(200, "OK", &call_id, 1, "b1");
        let ack = sip_request("ACK", &call_id, 1, "b1", "");
        let bye = sip_request("BYE", &call_id, 2, "b2", "");
        let ok_bye = sip_response_to(200, &call_id, 2, "BYE", "b2");
        let t0 = 1_000_000 + n * 3_000_000_000;
        let wire: Vec<(u64, bool, &[u8])> = vec![
            (t0, true, &inv),
            (t0 + 500_000, false, &ok),
            (t0 + 520_000, true, &ack),
            (t0 + 3_000_000, true, &bye),
            (t0 + 3_020_000, false, &ok_bye),
        ];
        for (ts, from_caller, payload) in wire {
            let (src, dst) = if from_caller {
                ("10.0.0.1:5060", "10.0.0.2:5060")
            } else {
                ("10.0.0.2:5060", "10.0.0.1:5060")
            };
            let far = if from_caller { ts + 7_000 } else { ts - 7_000 };
            datagrams.push(dgp(ts, src, dst, payload, 0));
            datagrams.push(dgp(far + offset, src, dst, payload, 1));
        }
    }
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.legs.len(), 2);
    for leg in &flows.legs {
        assert_eq!(
            methods(leg),
            vec!["INVITE", "200 INVITE", "ACK", "BYE", "200 BYE"],
            "{}",
            leg.call_id
        );
        assert!(leg.msgs.iter().all(|m| !m.retx), "{}", leg.call_id);
    }
    assert_eq!(flows.stats.capture_dups, 10);
    let stretches: Vec<_> = flows
        .stats
        .aligned_probes
        .iter()
        .map(|a| (a.probe, a.reference, a.from_us, a.offset_us, a.pairs))
        .collect();
    // The second stretch's offset is the median delta, the step plus the
    // transit in the majority direction (three requests to two responses).
    assert_eq!(
        stretches,
        vec![(1, 0, 0, 0, 5), (1, 0, 3_001_000_000 + 7_000 + 440_000, 447_000, 5)],
        "the first stretch is the transit alone, the second the step, from the first datagram it covers"
    );
}

/// Three probes chained pairwise — 0 shares datagrams with 1, 1 with 2 —
/// all land on the earliest clock, each offset composed along the chain.
#[test]
fn a_chain_of_probes_lands_on_the_earliest_clock() {
    let inv = sip_request("INVITE", "chain-1", 1, "b1", "");
    let trying = sip_response(100, "Trying", "chain-1", 1, "b1");
    let ok = sip_response(200, "OK", "chain-1", 1, "b1");
    let ack = sip_request("ACK", "chain-1", 1, "b1", "");
    let mut datagrams = Vec::new();
    // Probe 1 is the reference (earliest clock); probe 0 runs 300 ms ahead of
    // it and probe 2 runs 400 ms ahead of probe 0. Hop X is seen by 0 and 1,
    // hop Y by 0 and 2.
    let wire: Vec<(u64, bool, &[u8])> = vec![
        (1_000_000, true, &inv),
        (1_010_000, false, &trying),
        (1_100_000, false, &ok),
        (1_500_000, true, &ack),
    ];
    for (ts, from_caller, payload) in wire {
        let (src, dst) = if from_caller {
            ("10.0.0.1:5060", "10.0.0.2:5060")
        } else {
            ("10.0.0.2:5060", "10.0.0.1:5060")
        };
        datagrams.push(dgp(ts, src, dst, payload, 1));
        datagrams.push(dgp(ts + 300_000, src, dst, payload, 0));
        let (src, dst) = if from_caller {
            ("10.0.0.2:5062", "10.0.0.3:5060")
        } else {
            ("10.0.0.3:5060", "10.0.0.2:5062")
        };
        datagrams.push(dgp(ts + 20_000 + 300_000, src, dst, payload, 0));
        datagrams.push(dgp(ts + 20_000 + 700_000, src, dst, payload, 2));
    }
    let flows = build_flows(&datagrams, &FlowConfig::default());
    let leg = &flows.legs[0];
    assert_eq!(leg.msgs.len(), 8, "each hop's four messages once: {:?}", methods(leg));
    assert!(leg.msgs.iter().all(|m| !m.retx));
    let aligned: Vec<_> = flows
        .stats
        .aligned_probes
        .iter()
        .map(|a| (a.probe, a.reference, a.from_us, a.offset_us))
        .collect();
    assert_eq!(aligned, vec![(0, 1, 0, 300_000), (2, 1, 0, 700_000)]);
    assert_eq!(
        leg.msgs.iter().map(|m| m.ts_us).collect::<Vec<_>>(),
        vec![
            1_000_000, 1_010_000, 1_020_000, 1_030_000, 1_100_000, 1_120_000, 1_500_000, 1_520_000
        ]
    );
}

#[test]
fn non_sip_and_parse_failures_are_counted_not_ingested() {
    let datagrams = vec![
        dg(1_000, "10.0.0.1:9000", "10.0.0.2:9000", &[0x80; 64]), // RTP-ish
        dg(2_000, "10.0.0.1:5060", "10.0.0.2:5060", b"INVITE sip:x SIP/2.0\r\nGarbage\r\n\r\n"),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.stats.non_sip, 1);
    assert_eq!(flows.stats.parse_failed, 1);
    assert_eq!(flows.stats.parse_failed_at, vec![1]);
    assert_eq!(flows.stats.sip_messages, 0);
    assert!(flows.legs.is_empty());
}

#[test]
fn shared_token_groups_legs_with_evidence() {
    let a = sip_request("INVITE", "tok-a", 1, "ba", "X-Api-Call: call-77\r\n");
    let b = sip_request("INVITE", "tok-b", 1, "bb", "X-Api-Call: call-77\r\n");
    let lone = sip_request("INVITE", "tok-c", 1, "bc", "X-Api-Call: call-88\r\n");
    let datagrams = vec![
        dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
        dg(2_000, "10.0.0.2:5062", "10.0.0.9:5060", &b),
        dg(3_000, "10.0.0.7:5060", "10.0.0.8:5060", &lone),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.legs.len(), 3);
    assert_eq!(flows.groups.len(), 2);
    let joined = &flows.groups[0];
    assert_eq!(joined.legs, vec![0, 1]);
    assert_eq!(joined.evidence.len(), 1);
    let MatchEvidence::SharedToken { strategy, token, legs } = &joined.evidence[0] else {
        panic!("expected token evidence, got {:?}", joined.evidence)
    };
    assert_eq!(*strategy, 0);
    assert_eq!(token, "call-77");
    assert_eq!(legs, &vec![0, 1]);
    assert!(flows.groups[1].evidence.is_empty(), "single-leg group carries no evidence");
}

/// Token-less captures fall back to identity adjacency; the evidence
/// names the strategy, the crossing host and the pairing distance.
#[test]
fn identity_adjacency_fallback_records_evidence() {
    let a = sip_request("INVITE", "adj-a", 1, "ba", "");
    let b = sip_request("INVITE", "adj-b", 1, "bb", "");
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &a), // lands on the B2BUA
        dg(1_200_000, "10.0.0.5:5062", "10.0.0.9:5060", &b), // departs from it
    ];
    let cfg = FlowConfig::default();
    let flows = build_flows(&datagrams, &cfg);
    assert_eq!(flows.groups.len(), 1);
    let g = &flows.groups[0];
    assert_eq!(g.legs, vec![0, 1]);
    let MatchEvidence::IdentityAdjacency { strategy, legs, shared_host, dt_us } = &g.evidence[0]
    else {
        panic!("expected adjacency evidence, got {:?}", g.evidence)
    };
    assert_eq!(*strategy, 3, "identity adjacency is the default pipeline's last strategy");
    assert_eq!(legs, &[0, 1]);
    assert_eq!(*shared_host, "10.0.0.5".parse::<IpAddr>().unwrap());
    assert_eq!(*dt_us, 200_000);

    // Dropping the strategy from the pipeline disables the fallback.
    let token_only = FlowConfig {
        strategies: cfg
            .strategies
            .iter()
            .filter(|s| !matches!(s, CorrelateStrategy::IdentityAdjacency { .. }))
            .cloned()
            .collect(),
        ..FlowConfig::default()
    };
    let flows = build_flows(&datagrams, &token_only);
    assert_eq!(flows.groups.len(), 2, "fallback disabled → legs stay apart");
}

/// INVITE with fully custom R-URI / From / To (the cross-leg correlation
/// tests need every URI byte-different across the AS).
fn invite_custom(
    call_id: &str,
    branch: &str,
    ruri: &str,
    from: &str,
    to: &str,
    extra: &str,
) -> Vec<u8> {
    format!(
        "INVITE {ruri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             Max-Forwards: 70\r\n\
             From: {from};tag=f-{branch}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 INVITE\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

const ICID: &str = "icid-0001-example-5";

/// A B2BUA in the path that rewrites every URI (hosts and params
/// rewritten, `tel:` vs `sip:` forms), while both INVITEs carry the
/// same P-Charging-Vector icid-value among mutating sibling params — the
/// header-param strategy pairs the legs and the evidence names it.
#[test]
fn header_param_strategy_pairs_b2bua_legs_on_icid() {
    let a = invite_custom(
        "b2b-a",
        "ba",
        "sip:+15550900011@foo.example.com;user=phone",
        "\"SiteA\" <sip:+15550900012;verstat=TN-Validation-Passed@bar.example.com:5060;user=phone>",
        "<tel:+15550900011>",
        &format!("P-Charging-Vector: icid-value={ICID};icid-generated-at=198.51.100.63\r\n"),
    );
    let b = invite_custom(
        "b2b-b",
        "bb",
        "sip:+15550900011@ims.mnc001.example;user=phone",
        "<sip:+15550900012;verstat=No-TN-Validation@baz.example.com;user=phone>",
        "<sip:+15550900011@ims.mnc001.example>",
        // Param order variance + mutated siblings: only the icid matches.
        &format!("P-Charging-Vector: orig-ioi=op2.example;icid-value={ICID}\r\n"),
    );
    let datagrams = vec![
        dg(1_000_000, "198.51.100.41:5092", "198.51.100.226:5090", &a),
        dg(1_100_000, "198.51.100.226:54448", "10.20.0.1:5060", &b),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 1, "icid must pair the B2BUA legs");
    let g = &flows.groups[0];
    assert_eq!(g.legs, vec![0, 1]);
    assert_eq!(g.evidence.len(), 1);
    let MatchEvidence::SharedHeaderParam { strategy, header, param, token, legs } = &g.evidence[0]
    else {
        panic!("expected header-param evidence, got {:?}", g.evidence)
    };
    assert_eq!(*strategy, 1, "P-Charging-Vector icid is the default pipeline's second strategy");
    assert_eq!(header, "P-Charging-Vector");
    assert_eq!(param, "icid-value");
    assert_eq!(token, ICID);
    assert_eq!(legs, &vec![0, 1]);
}

/// A multi-hop relayed a-leg only reaches the AS host at its LAST
/// observed hop, and the b-leg departs the AS from an ephemeral port —
/// the first-observed socket pairs do not cross, so any-hop (IP-level)
/// crossing is what pairs them.
#[test]
fn identity_adjacency_crosses_at_any_traversed_hop() {
    let a = invite_custom(
        "multihop-a",
        "ba",
        "sip:+15550002@as.example",
        "<sip:+15550001@enterprise.example;user=phone>",
        "<tel:+15550002>",
        "",
    );
    let b = invite_custom(
        "multihop-b",
        "bb",
        "sip:+15550002@term.example",
        "<tel:+15550001>",
        "<sip:+15550002@term.example>",
        "",
    );
    let datagrams = vec![
        // a-leg relayed across three hops; the AS (10.0.0.9) appears only
        // as the destination of the LAST one.
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
        dg(1_010_000, "10.0.0.2:5071", "10.0.0.3:5060", &a),
        dg(1_020_000, "10.0.0.3:5092", "10.0.0.9:5090", &a),
        // b-leg departs the AS from an ephemeral port.
        dg(1_200_000, "10.0.0.9:54448", "10.0.0.7:5060", &b),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.legs.len(), 2);
    // The shape genuinely defeats first-socket-pair crossing: neither
    // first-observed INVITE (src, dst) shares an IP the right way round.
    let (sa, da) = flows.legs[0].invite_addrs().unwrap();
    let (sb, db) = flows.legs[1].invite_addrs().unwrap();
    assert_ne!(da.ip(), sb.ip(), "first-pair crossing would already match — shape is wrong");
    assert_ne!(db.ip(), sa.ip(), "first-pair crossing would already match — shape is wrong");

    assert_eq!(flows.groups.len(), 1, "any-hop crossing must pair the legs");
    let g = &flows.groups[0];
    assert_eq!(g.legs, vec![0, 1]);
    let MatchEvidence::IdentityAdjacency { strategy, legs, shared_host, .. } = &g.evidence[0]
    else {
        panic!("expected adjacency evidence, got {:?}", g.evidence)
    };
    assert_eq!(*strategy, 3);
    assert_eq!(legs, &[0, 1]);
    assert_eq!(*shared_host, "10.0.0.9".parse::<IpAddr>().unwrap());
}

/// Identity adjacency is the gated exception to "every attesting strategy
/// records": when a relayed token already paired the legs, the fallback
/// stays silent rather than asserting an identity match on weaker ground.
#[test]
fn identity_adjacency_stays_silent_once_a_token_paired_the_legs() {
    let a = sip_request("INVITE", "ord-a", 1, "ba", "X-Api-Call: call-42\r\n");
    let b = sip_request("INVITE", "ord-b", 1, "bb", "X-Api-Call: call-42\r\n");
    let datagrams = vec![
        // Adjacency shape too: same From/To users, crossing 10.0.0.5.
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &a),
        dg(1_200_000, "10.0.0.5:5062", "10.0.0.9:5060", &b),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 1);
    let g = &flows.groups[0];
    assert_eq!(g.evidence.len(), 1, "one join → one evidence entry: {:?}", g.evidence);
    let MatchEvidence::SharedToken { strategy, token, .. } = &g.evidence[0] else {
        panic!("expected the earlier (token) strategy, got {:?}", g.evidence)
    };
    assert_eq!(*strategy, 0);
    assert_eq!(token, "call-42");
}

/// Two strategies that INDEPENDENTLY attest the same pairing both record
/// it: the icid merges the legs, and the Call-ID derivation — which alone
/// names which socket is the application server — must still be attested,
/// or a consumer resolving the loopback from evidence goes blind.
#[test]
fn every_attesting_strategy_records_its_evidence() {
    let icid = format!("P-Charging-Vector: icid-value={ICID}\r\n");
    let base = sip_request("INVITE", "SDq1u9502-36a5c334-v300g00040", 1, "ba", &icid);
    let derived = sip_request("INVITE", "1-SDq1u9502-36a5c334-v300g00040", 1, "bb", &icid);
    let datagrams = vec![
        dg(1_000_000, "192.0.2.9:5061", "192.0.2.67:5060", &base),
        dg(1_040_000, "192.0.2.67:5060", "192.0.2.9:5061", &derived),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 1);
    let g = &flows.groups[0];
    assert_eq!(g.evidence.len(), 2, "icid AND derivation must both record: {:?}", g.evidence);
    assert!(matches!(&g.evidence[0], MatchEvidence::SharedHeaderParam { strategy: 1, .. }));
    let MatchEvidence::DerivedCallId { strategy: 2, as_socket, peer_socket, .. } = &g.evidence[1]
    else {
        panic!("the derivation must be attested even though the icid merged first")
    };
    assert_eq!(as_socket.to_string(), "192.0.2.67:5060");
    assert_eq!(peer_socket.to_string(), "192.0.2.9:5061");
}

/// No over-grouping: different icids, different users, no crossing —
/// the legs stay apart.
#[test]
fn unrelated_legs_are_not_paired() {
    let a = invite_custom(
        "neg-a",
        "ba",
        "sip:+15550002@x.example",
        "<sip:+15550001@x.example>",
        "<sip:+15550002@x.example>",
        "P-Charging-Vector: icid-value=icid-aaa;orig-ioi=x\r\n",
    );
    let b = invite_custom(
        "neg-b",
        "bb",
        "sip:+15559999@y.example",
        "<sip:+15558888@y.example>",
        "<sip:+15559999@y.example>",
        "P-Charging-Vector: icid-value=icid-bbb;orig-ioi=y\r\n",
    );
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
        dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 2, "unrelated legs must not group");
    assert!(flows.groups.iter().all(|g| g.evidence.is_empty()));
}

/// Two legs that BOTH carry values for the same token strategy, with no
/// value shared, are affirmatively different calls — identity adjacency
/// must not pair them even though users, crossing and window all match
/// (the token-less shape of this exact scenario pairs, see
/// `earlier_strategy_wins_and_is_the_only_evidence`).
#[test]
fn disjoint_tokens_veto_identity_adjacency() {
    let a = sip_request("INVITE", "veto-a", 1, "ba", "X-Api-Call: call-A\r\n");
    let b = sip_request("INVITE", "veto-b", 1, "bb", "X-Api-Call: call-B\r\n");
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &a),
        dg(1_200_000, "10.0.0.5:5062", "10.0.0.9:5060", &b),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 2, "disagreeing tokens must veto the identity pairing");
    assert!(flows.groups.iter().all(|g| g.evidence.is_empty()));
}

/// A one-sided (unechoed) token is NOT a veto: the token-carrying leg
/// still identity-pairs with a token-less peer.
#[test]
fn one_sided_token_does_not_veto_adjacency() {
    let a = sip_request("INVITE", "oneside-a", 1, "ba", "X-Api-Call: call-A\r\n");
    let b = sip_request("INVITE", "oneside-b", 1, "bb", "");
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &a),
        dg(1_200_000, "10.0.0.5:5062", "10.0.0.9:5060", &b),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 1, "an unechoed token must not block the pairing");
    assert!(matches!(
        &flows.groups[0].evidence[0],
        MatchEvidence::IdentityAdjacency { strategy: 3, legs: [0, 1], .. }
    ));
}

/// An empty (`icid-value=`) or flag (`icid-value`) param never becomes a
/// correlation token — the ingest guard drops it.
#[test]
fn empty_or_flag_icid_never_correlates() {
    let a = invite_custom(
        "emptyicid-a",
        "ba",
        "sip:+15550002@x.example",
        "<sip:+15550001@x.example>",
        "<sip:+15550002@x.example>",
        "P-Charging-Vector: icid-value=;orig-ioi=x\r\n",
    );
    let b = invite_custom(
        "emptyicid-b",
        "bb",
        "sip:+15559999@y.example",
        "<sip:+15558888@y.example>",
        "<sip:+15559999@y.example>",
        "P-Charging-Vector: icid-value\r\n",
    );
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
        dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert!(flows.legs.iter().all(|l| l.tokens().is_empty()), "empty values are not tokens");
    assert_eq!(flows.groups.len(), 2);
}

/// An icid on only one leg pairs nothing — strategy 2 needs the value on
/// both sides.
#[test]
fn icid_on_one_leg_only_does_not_pair() {
    let a = invite_custom(
        "loneicid-a",
        "ba",
        "sip:+15550002@x.example",
        "<sip:+15550001@x.example>",
        "<sip:+15550002@x.example>",
        &format!("P-Charging-Vector: icid-value={ICID};orig-ioi=x\r\n"),
    );
    let b = invite_custom(
        "loneicid-b",
        "bb",
        "sip:+15559999@y.example",
        "<sip:+15558888@y.example>",
        "<sip:+15559999@y.example>",
        "",
    );
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
        dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 2);
    assert!(flows.groups.iter().all(|g| g.evidence.is_empty()));
}

/// One icid seen on three legs joins all of them into one group, and the
/// evidence lists every carrier.
#[test]
fn three_legs_sharing_one_icid_form_one_group() {
    let pcv = format!("P-Charging-Vector: icid-value={ICID}\r\n");
    let a = sip_request("INVITE", "tri-a", 1, "ba", &pcv);
    let b = sip_request("INVITE", "tri-b", 1, "bb", &pcv);
    let c = sip_request("INVITE", "tri-c", 1, "bc", &pcv);
    let datagrams = vec![
        // Non-crossing addresses: only the icid can group these.
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
        dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
        dg(1_200_000, "10.0.0.5:5060", "10.0.0.6:5060", &c),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 1);
    assert_eq!(flows.groups[0].legs, vec![0, 1, 2]);
    assert_eq!(flows.groups[0].evidence.len(), 1);
    let MatchEvidence::SharedHeaderParam { legs, .. } = &flows.groups[0].evidence[0] else {
        panic!("expected header-param evidence, got {:?}", flows.groups[0].evidence)
    };
    assert_eq!(legs, &vec![0, 1, 2]);
}

/// The application-server loopback: the AS answers the base leg and
/// re-originates it as `1-<original>` back out the same socket pair. The
/// evidence names the AS socket and the peer it looped back to.
#[test]
fn derived_call_id_pairs_an_as_loopback() {
    let base = sip_request("INVITE", "SDq1u9502-36a5c334-v300g00040", 1, "ba", "");
    let derived = sip_request("INVITE", "1-SDq1u9502-36a5c334-v300g00040", 1, "bb", "");
    let datagrams = vec![
        dg(1_000_000, "192.0.2.9:5061", "192.0.2.67:5060", &base),
        dg(1_040_000, "192.0.2.67:5060", "192.0.2.9:5061", &derived),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 1, "the loopback must pair the legs");
    let g = &flows.groups[0];
    assert_eq!(g.legs, vec![0, 1]);
    let MatchEvidence::DerivedCallId {
        strategy,
        legs,
        prefix,
        as_socket,
        peer_socket,
        shared_hop,
        dt_us,
    } = &g.evidence[0]
    else {
        panic!("expected derivation evidence, got {:?}", g.evidence)
    };
    assert_eq!(*strategy, 2, "Call-ID derivation is the default pipeline's third strategy");
    assert_eq!(legs, &[0, 1]);
    assert_eq!(prefix, "1-");
    assert_eq!(as_socket.to_string(), "192.0.2.67:5060", "the AS emitted the derived INVITE");
    assert_eq!(peer_socket.to_string(), "192.0.2.9:5061");
    assert!(*shared_hop);
    assert_eq!(*dt_us, 40_000);
}

/// The derivation alone is not enough: a same-named leg that never
/// departs on a hop the base leg traversed is a different call — until
/// the shared-hop requirement is lifted, which the evidence then records.
#[test]
fn derived_call_id_without_a_shared_hop_needs_the_relaxed_strategy() {
    let base = sip_request("INVITE", "SDq1u9502-36a5c334-v300g00040", 1, "ba", "");
    let derived = sip_request("INVITE", "1-SDq1u9502-36a5c334-v300g00040", 1, "bb", "");
    let datagrams = vec![
        dg(1_000_000, "192.0.2.9:5061", "192.0.2.67:5060", &base),
        dg(1_040_000, "10.20.9.1:5060", "10.20.9.2:5060", &derived),
    ];
    let strict = FlowConfig {
        strategies: vec![CorrelateStrategy::DerivedCallId {
            window_us: DEFAULT_PAIR_WINDOW_US,
            require_shared_hop: true,
            min_base_len: 8,
        }],
        ..FlowConfig::default()
    };
    assert_eq!(build_flows(&datagrams, &strict).groups.len(), 2, "no loopback → no pairing");

    let relaxed = FlowConfig {
        strategies: vec![CorrelateStrategy::DerivedCallId {
            window_us: DEFAULT_PAIR_WINDOW_US,
            require_shared_hop: false,
            min_base_len: 8,
        }],
        ..FlowConfig::default()
    };
    let flows = build_flows(&datagrams, &relaxed);
    assert_eq!(flows.groups.len(), 1);
    let MatchEvidence::DerivedCallId { shared_hop, as_socket, .. } = &flows.groups[0].evidence[0]
    else {
        panic!("expected derivation evidence")
    };
    assert!(!shared_hop, "the pairing must not claim a loopback it did not see");
    assert_eq!(as_socket.to_string(), "10.20.9.1:5060");
}

/// Two guards against a coincidental suffix: a base Call-ID under
/// `min_base_len`, and a derived leg outside the pairing window. Asserted
/// on a derivation-only pipeline — a later strategy pairing these legs on
/// other evidence would mask the guard, not exercise it.
#[test]
fn short_call_ids_and_distant_legs_never_derive() {
    let only_derivation = FlowConfig {
        strategies: vec![CorrelateStrategy::DerivedCallId {
            window_us: DEFAULT_PAIR_WINDOW_US,
            require_shared_hop: true,
            min_base_len: 8,
        }],
        ..FlowConfig::default()
    };
    let base = sip_request("INVITE", "abc", 1, "ba", "");
    let derived = sip_request("INVITE", "xyzabc", 1, "bb", "");
    let short = vec![
        dg(1_000_000, "192.0.2.9:5061", "192.0.2.67:5060", &base),
        dg(1_040_000, "192.0.2.67:5060", "192.0.2.9:5061", &derived),
    ];
    let flows = build_flows(&short, &only_derivation);
    assert_eq!(flows.groups.len(), 2, "a 3-char base Call-ID must not derive");

    let base = sip_request("INVITE", "SDq1u9502-36a5c334-v300g00040", 1, "ba", "");
    let derived = sip_request("INVITE", "1-SDq1u9502-36a5c334-v300g00040", 1, "bb", "");
    let distant = vec![
        dg(1_000_000, "192.0.2.9:5061", "192.0.2.67:5060", &base),
        dg(9_000_000, "192.0.2.67:5060", "192.0.2.9:5061", &derived),
    ];
    let flows = build_flows(&distant, &only_derivation);
    assert_eq!(flows.groups.len(), 2, "8s apart is past the 5s pairing window");
}

/// A chain of application servers (`X` → `1-X` → `1-1-X`) collapses into
/// one group, each hop recorded with its own AS socket.
#[test]
fn chained_derivations_form_one_group() {
    let a = sip_request("INVITE", "SDq1u9502-36a5c334-v300g00040", 1, "ba", "");
    let b = sip_request("INVITE", "1-SDq1u9502-36a5c334-v300g00040", 1, "bb", "");
    let c = sip_request("INVITE", "1-1-SDq1u9502-36a5c334-v300g00040", 1, "bc", "");
    let datagrams = vec![
        dg(1_000_000, "192.0.2.9:5061", "192.0.2.67:5060", &a),
        dg(1_040_000, "192.0.2.67:5060", "192.0.2.9:5061", &b),
        dg(1_080_000, "192.0.2.9:5061", "192.0.2.67:5060", &c),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 1);
    assert_eq!(flows.groups[0].legs, vec![0, 1, 2]);
    assert!(flows.groups[0]
        .evidence
        .iter()
        .all(|e| matches!(e, MatchEvidence::DerivedCallId { .. })));
}

/// A later strategy can still join a NOT-yet-paired leg into a group an
/// earlier strategy formed (the partial-overlap union path): the group
/// carries both strategies' evidence, each naming the legs it joined.
#[test]
fn later_strategy_joins_unpaired_leg_into_earlier_group() {
    let icid = format!("P-Charging-Vector: icid-value={ICID}\r\n");
    let a = sip_request("INVITE", "join-a", 1, "ba", "X-Api-Call: call-7\r\n");
    let b = sip_request("INVITE", "join-b", 1, "bb", &format!("X-Api-Call: call-7\r\n{icid}"));
    let c = sip_request("INVITE", "join-c", 1, "bc", &icid);
    let datagrams = vec![
        // Non-crossing addresses: only the tokens can group these.
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
        dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
        dg(1_200_000, "10.0.0.5:5060", "10.0.0.6:5060", &c),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    assert_eq!(flows.groups.len(), 1);
    let g = &flows.groups[0];
    assert_eq!(g.legs, vec![0, 1, 2]);
    assert_eq!(g.evidence.len(), 2);
    let MatchEvidence::SharedToken { strategy: 0, legs: tok_legs, .. } = &g.evidence[0] else {
        panic!("expected token evidence first, got {:?}", g.evidence)
    };
    assert_eq!(tok_legs, &vec![0, 1]);
    let MatchEvidence::SharedHeaderParam { strategy: 1, legs: icid_legs, .. } = &g.evidence[1]
    else {
        panic!("expected header-param evidence second, got {:?}", g.evidence)
    };
    assert_eq!(icid_legs, &vec![1, 2]);
}
