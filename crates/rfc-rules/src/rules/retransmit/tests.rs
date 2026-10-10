//! The rule's OWN semantics under a CLOSED observation: which copies form
//! one message, and what the bytes alone settle about them.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::RungByteIdentical;

const UAC: &str = "10.0.0.1:5060";
const UAS: &str = "10.0.0.2:5060";

/// A response the UAS sent, its bytes given in full.
fn rsp(at_us: u64, status: u16, method: &str, to_tag: &str, raw: &str) -> Msg {
    Msg {
        at_us,
        src: UAS.to_string(),
        dst: UAC.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status },
        call_id: "c1".to_string(),
        cseq: 1,
        cseq_method: method.to_string(),
        from_tag: Some("fa".to_string()),
        to_tag: (!to_tag.is_empty()).then(|| to_tag.to_string()),
        via_branch: Some("z9hG4bK-1".to_string()),
        head: Some(raw.as_bytes().to_vec()),
        body: sip_message::sniff::body(raw.as_bytes()).map(<[u8]>::to_vec),
    }
}

/// A request the UAC sent, its bytes given in full.
fn req(at_us: u64, method: &str, branch: &str, raw: &str) -> Msg {
    Msg {
        at_us,
        src: UAC.to_string(),
        dst: UAS.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "c1".to_string(),
        cseq: 1,
        cseq_method: method.to_string(),
        from_tag: Some("fa".to_string()),
        to_tag: None,
        via_branch: Some(branch.to_string()),
        head: Some(raw.as_bytes().to_vec()),
        body: sip_message::sniff::body(raw.as_bytes()).map(<[u8]>::to_vec),
    }
}

/// A 200 (INVITE), headers in the order given.
fn ok_200(rows: &[&str]) -> String {
    format!("SIP/2.0 200 OK\r\n{}\r\n\r\n", rows.join("\r\n"))
}

const ROWS: [&str; 6] = [
    "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1",
    "From: <sip:a@h>;tag=fa",
    "To: <sip:b@h>;tag=tb",
    "Call-ID: c1",
    "CSeq: 1 INVITE",
    "Content-Length: 0",
];

fn obs(msgs: &[Msg]) -> Observation {
    let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
    let mut last_us = 0;
    for m in msgs {
        last_us = last_us.max(m.at_us);
        for ep in [&m.src, &m.dst] {
            let at = endpoint_last_us.entry(ep.clone()).or_default();
            *at = (*at).max(m.at_us);
        }
    }
    Observation { last_us, endpoint_last_us, closed: true }
}

fn eval(msgs: &[Msg]) -> Vec<Finding> {
    RungByteIdentical.eval(&WireView { msgs, obs: &obs(msgs) })
}

fn violated(f: &Finding) -> &Evidence {
    match &f.decision {
        Decision::Violated(e) => e,
        other => panic!("expected a violation, got {other:?}"),
    }
}

/// The violation: a 2xx whose repeat was re-composed — the
/// same headers, two of them swapped — is not the same response.
#[test]
fn a_re_composed_2xx_rung_is_violated() {
    let first = ok_200(&ROWS);
    let mut swapped = ROWS;
    swapped.swap(4, 5);
    let rung = ok_200(&swapped);
    let f =
        eval(&[rsp(1_000, 200, "INVITE", "tb", &first), rsp(501_000, 200, "INVITE", "tb", &rung)]);
    assert_eq!(f.len(), 1, "one occasion, the message: {f:?}");
    assert_eq!(f[0].rule, RuleId::RungByteIdentical);
    assert_eq!(f[0].emitter, UAS, "the endpoint that re-composed its rung is charged");
    assert_eq!(f[0].taker, UAC);
    assert_eq!(f[0].anchor, 1, "the occasion rests on the divergent copy");
    let Evidence::RungDiverged {
        rung,
        copies,
        divergent,
        class,
        method,
        status,
        rseq,
        branch,
        region,
        offset,
        first_line,
        rung_line,
        gap_us,
        ..
    } = violated(&f[0])
    else {
        panic!("rung-diverged evidence: {:?}", f[0].decision)
    };
    assert_eq!((*rung, *copies, *divergent), (1, 2, 1));
    assert_eq!(class, "2xx final");
    assert_eq!((method.as_str(), *status, *rseq), ("INVITE", Some(200), None));
    assert_eq!(branch, "z9hG4bK-1");
    assert_eq!(region, "head");
    assert_eq!(*offset, first.find("CSeq: 1").unwrap() + 1, "the first byte the two disagree on");
    assert_eq!(first_line.as_deref(), Some("CSeq: 1 INVITE"));
    assert_eq!(rung_line.as_deref(), Some("Content-Length: 0"));
    assert_eq!(*gap_us, 500_000);
}

/// A normalised value is a different byte: `Content-Length:0` is not
/// `Content-Length: 0`, whatever a parser makes of both.
#[test]
fn a_normalised_value_on_a_rung_is_violated() {
    let first = ok_200(&ROWS);
    let rung = first.replace("Content-Length: 0", "Content-Length:0");
    let f =
        eval(&[rsp(1_000, 200, "INVITE", "tb", &first), rsp(501_000, 200, "INVITE", "tb", &rung)]);
    assert_eq!(f.len(), 1);
    assert!(f[0].violated(), "{f:?}");
}

/// The obliged behaviour: the same bytes again, marked a repeat by the
/// adapter or not, is one compliant occasion.
#[test]
fn a_byte_identical_rung_is_compliant() {
    let raw = ok_200(&ROWS);
    let mut marked = rsp(501_000, 200, "INVITE", "tb", &raw);
    marked.repeat = true;
    let f = eval(&[
        rsp(1_000, 200, "INVITE", "tb", &raw),
        marked,
        rsp(1_501_000, 200, "INVITE", "tb", &raw),
    ]);
    assert_eq!(f.len(), 1, "one occasion however many rungs: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{f:?}");
    assert_eq!(f[0].anchor, 1, "a compliant occasion rests on the second copy");
}

#[test]
fn a_single_emission_is_no_occasion() {
    assert!(eval(&[rsp(1_000, 200, "INVITE", "tb", &ok_200(&ROWS))]).is_empty());
}

/// Two forks answer the one INVITE under the one branch and CSeq; their tags
/// are what says they are two messages, and no bytes are compared.
#[test]
fn a_forks_2xx_is_not_a_repeat_of_another_forks() {
    let winner = ok_200(&ROWS);
    let mut rows = ROWS;
    rows[2] = "To: <sip:b@h>;tag=tc";
    let loser = ok_200(&rows);
    let f =
        eval(&[rsp(1_000, 200, "INVITE", "tb", &winner), rsp(2_000, 200, "INVITE", "tc", &loser)]);
    assert!(f.is_empty(), "{f:?}");
}

/// A different status on the one transaction is the single-final rule's
/// occasion, not a rung of anything.
#[test]
fn a_second_final_of_another_status_is_not_a_rung() {
    let first = ok_200(&ROWS).replace("200 OK", "487 Terminated");
    let second = ok_200(&ROWS).replace("200 OK", "480 Gone");
    let f =
        eval(&[rsp(1_000, 487, "INVITE", "tb", &first), rsp(2_000, 480, "INVITE", "tb", &second)]);
    assert!(f.is_empty(), "{f:?}");
}

fn reliable_183(rseq: u32, extra: &str) -> String {
    format!(
        "SIP/2.0 183 Session Progress\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:a@h>;tag=fa\r\n\
             To: <sip:b@h>;tag=tb\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Require: 100rel\r\n\
             RSeq: {rseq}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    )
}

/// RFC 3262 §3 sequences distinct provisionals through one transaction:
/// two 183 that differ in `RSeq` are two messages.
#[test]
fn two_reliable_provisionals_with_distinct_rseq_are_two_messages() {
    let f = eval(&[
        rsp(1_000, 183, "INVITE", "tb", &reliable_183(1, "")),
        rsp(2_000, 183, "INVITE", "tb", &reliable_183(2, "P-Early-Media: sendrecv\r\n")),
    ]);
    assert!(f.is_empty(), "{f:?}");
}

/// The same `RSeq` again with other bytes is a re-composed rung of the one
/// reliable provisional.
#[test]
fn a_re_composed_reliable_provisional_rung_is_violated() {
    let f = eval(&[
        rsp(1_000, 183, "INVITE", "tb", &reliable_183(1, "")),
        rsp(501_000, 183, "INVITE", "tb", &reliable_183(1, "P-Early-Media: sendrecv\r\n")),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Evidence::RungDiverged { class, rseq, status, .. } = violated(&f[0]) else { panic!() };
    assert_eq!((class.as_str(), *rseq, *status), ("reliable provisional", Some(1), Some(183)));
}

/// An unreliable provisional rides no timer and §13.3.1.1 lets the UAS
/// send as many distinct ones as it likes — two 183, or two 100, on one
/// transaction are two messages and never an occasion.
#[test]
fn an_unreliable_provisional_is_never_an_occasion() {
    let ringing = |extra: &str| {
        format!(
            "SIP/2.0 183 Session Progress\r\n\
                 Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
                 To: <sip:b@h>;tag=tb\r\n\
                 CSeq: 1 INVITE\r\n{extra}Content-Length: 0\r\n\r\n"
        )
    };
    let f = eval(&[
        rsp(1_000, 183, "INVITE", "tb", &ringing("")),
        rsp(501_000, 183, "INVITE", "tb", &ringing("P-Early-Media: sendrecv\r\n")),
    ]);
    assert!(f.is_empty(), "{f:?}");
    let trying = |extra: &str| format!("SIP/2.0 100 Trying\r\nCSeq: 1 INVITE\r\n{extra}\r\n");
    let f = eval(&[
        rsp(1_000, 100, "INVITE", "", &trying("")),
        rsp(501_000, 100, "INVITE", "", &trying("Timestamp: 2\r\n")),
    ]);
    assert!(f.is_empty(), "{f:?}");
}

fn invite(branch: &str, max_forwards: u8) -> String {
    format!(
        "INVITE sip:b@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:a@h>;tag=fa\r\n\
             To: <sip:b@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: {max_forwards}\r\n\
             Content-Length: 0\r\n\r\n"
    )
}

/// The client transaction retransmits THE request (§17.1.1.2): a copy
/// under the same branch with another byte is charged to the UAC.
#[test]
fn a_re_composed_request_rung_is_violated() {
    let f = eval(&[
        req(1_000, "INVITE", "z9hG4bK-1", &invite("z9hG4bK-1", 70)),
        req(501_000, "INVITE", "z9hG4bK-1", &invite("z9hG4bK-1", 69)),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, UAC);
    let Evidence::RungDiverged { class, method, status, first_line, rung_line, .. } =
        violated(&f[0])
    else {
        panic!()
    };
    assert_eq!((class.as_str(), method.as_str(), *status), ("INVITE request", "INVITE", None));
    assert_eq!(first_line.as_deref(), Some("Max-Forwards: 70"));
    assert_eq!(rung_line.as_deref(), Some("Max-Forwards: 69"));
}

/// A new branch is a new transaction (§17.1.3): the same CSeq re-sent under
/// it is compared with nothing.
#[test]
fn a_request_under_a_new_branch_is_a_new_transaction() {
    let f = eval(&[
        req(1_000, "INVITE", "z9hG4bK-1", &invite("z9hG4bK-1", 70)),
        req(2_000, "INVITE", "z9hG4bK-2", &invite("z9hG4bK-2", 69)),
    ]);
    assert!(f.is_empty(), "{f:?}");
}

fn ack(branch: &str, extra: &str) -> String {
    format!(
        "ACK sip:b@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:a@h>;tag=fa\r\n\
             To: <sip:b@h>;tag=tb\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 ACK\r\n{extra}\
             Content-Length: 0\r\n\r\n"
    )
}

/// An ACK rides no timer, but the one re-passed on a repeated final is THE
/// ACK (§17.1.1.2): other bytes under the same branch are charged. A fresh
/// ACK under a new branch is its own transaction (§17.1.1.3) and no rung.
#[test]
fn an_ack_re_passed_with_other_bytes_is_violated_and_a_new_branch_is_not() {
    let f = eval(&[
        req(1_000, "ACK", "z9hG4bK-a", &ack("z9hG4bK-a", "")),
        req(501_000, "ACK", "z9hG4bK-a", &ack("z9hG4bK-a", "Max-Forwards: 70\r\n")),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Evidence::RungDiverged { class, .. } = violated(&f[0]) else { panic!() };
    assert_eq!(class, "ACK");
    let f = eval(&[
        req(1_000, "ACK", "z9hG4bK-a", &ack("z9hG4bK-a", "")),
        req(501_000, "ACK", "z9hG4bK-b", &ack("z9hG4bK-b", "Max-Forwards: 70\r\n")),
    ]);
    assert!(f.is_empty(), "{f:?}");
}

/// A non-INVITE final is re-passed on each repeated request (§17.2.2) —
/// the same final, so a re-composed one is charged like any other.
#[test]
fn a_re_composed_non_invite_final_is_violated() {
    let bye_ok = |extra: &str| {
        format!(
            "SIP/2.0 200 OK\r\n\
                 Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
                 To: <sip:b@h>;tag=tb\r\n\
                 Call-ID: c1\r\n\
                 CSeq: 1 BYE\r\n{extra}Content-Length: 0\r\n\r\n"
        )
    };
    let f = eval(&[
        rsp(1_000, 200, "BYE", "tb", &bye_ok("")),
        rsp(501_000, 200, "BYE", "tb", &bye_ok("Server: x\r\n")),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Evidence::RungDiverged { class, method, .. } = violated(&f[0]) else { panic!() };
    assert_eq!((class.as_str(), method.as_str()), ("non-INVITE final", "BYE"));
}

/// Past 64·T1 the transaction is gone: a copy under the old identity opens
/// a ladder of its own and repeats nothing.
#[test]
fn past_the_envelope_a_matching_identity_is_a_fresh_message() {
    let first = ok_200(&ROWS);
    let later = first.replace("Content-Length: 0", "Content-Length:0");
    let f = eval(&[
        rsp(1_000, 200, "INVITE", "tb", &first),
        rsp(33_001_000, 200, "INVITE", "tb", &later),
    ]);
    assert!(f.is_empty(), "{f:?}");
}

/// Without a branch the copies cannot be split into transactions, so a
/// divergence is undecidable; identical bytes are compliant regardless.
#[test]
fn without_a_branch_a_divergent_copy_is_undecidable() {
    let first = ok_200(&ROWS);
    let rung = first.replace("Content-Length: 0", "Content-Length:0");
    let mut a = rsp(1_000, 200, "INVITE", "tb", &first);
    let mut b = rsp(501_000, 200, "INVITE", "tb", &rung);
    a.via_branch = None;
    b.via_branch = None;
    let f = eval(&[a.clone(), b]);
    assert_eq!(f.len(), 1);
    assert!(
        matches!(f[0].decision, Decision::Undecidable("no via branch at this vantage")),
        "{f:?}"
    );
    let mut same = a.clone();
    same.at_us = 501_000;
    let f = eval(&[a, same]);
    assert!(matches!(f[0].decision, Decision::Compliant), "{f:?}");
}

/// A copy the vantage carried no bytes for can be compared with nothing.
#[test]
fn without_bytes_a_copy_is_undecidable() {
    let first = ok_200(&ROWS);
    let mut opaque = rsp(501_000, 200, "INVITE", "tb", &first);
    opaque.head = None;
    opaque.body = None;
    let f = eval(&[rsp(1_000, 200, "INVITE", "tb", &first), opaque]);
    assert_eq!(f.len(), 1);
    assert!(matches!(f[0].decision, Decision::Undecidable("no bytes at this vantage")), "{f:?}");
}

fn ok_200_sdp(port: u16) -> String {
    let body = format!("v=0\r\no=- 1 1 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio {port} RTP/AVP 0\r\n");
    let mut rows = ROWS.to_vec();
    rows.pop();
    rows.push("Content-Type: application/sdp");
    let len = format!("Content-Length: {}", body.len());
    rows.push(&len);
    format!("SIP/2.0 200 OK\r\n{}\r\n\r\n{body}", rows.join("\r\n"))
}

/// A body re-rendered on the rung is located in the body, whether the
/// vantage stored the datagram whole (the live recording) or split the
/// body off its head (a capture with a binary part).
#[test]
fn a_body_divergence_is_located_in_the_body_at_either_vantage() {
    let first = ok_200_sdp(4000);
    let rung = ok_200_sdp(4002);
    let whole =
        eval(&[rsp(1_000, 200, "INVITE", "tb", &first), rsp(501_000, 200, "INVITE", "tb", &rung)]);
    assert_eq!(whole.len(), 1);
    let Evidence::RungDiverged { region, offset, first_line, rung_line, .. } = violated(&whole[0])
    else {
        panic!()
    };
    let (whole_region, whole_offset) = (region.clone(), *offset);
    assert_eq!(region, "body");
    assert_eq!(first_line.as_deref(), Some("m=audio 4000 RTP/AVP 0"));
    assert_eq!(rung_line.as_deref(), Some("m=audio 4002 RTP/AVP 0"));

    let split = |mut m: Msg| {
        let head = m.head.take().unwrap();
        let body_len = m.body.as_ref().unwrap().len();
        m.head = Some(head[..head.len() - body_len].to_vec());
        m
    };
    let f = eval(&[
        split(rsp(1_000, 200, "INVITE", "tb", &first)),
        split(rsp(501_000, 200, "INVITE", "tb", &rung)),
    ]);
    let Evidence::RungDiverged { region, offset, .. } = violated(&f[0]) else { panic!() };
    assert_eq!(
        (region.as_str(), *offset),
        (whole_region.as_str(), whole_offset),
        "one region, one offset, either way"
    );
}

/// One occasion per message: the finding rests on the FIRST divergent copy
/// and counts every copy and every divergence on the ladder.
#[test]
fn every_divergent_copy_is_counted_on_one_finding() {
    let first = ok_200(&ROWS);
    let other = first.replace("Content-Length: 0", "Content-Length:0");
    let f = eval(&[
        rsp(1_000, 200, "INVITE", "tb", &first),
        rsp(501_000, 200, "INVITE", "tb", &first),
        rsp(1_501_000, 200, "INVITE", "tb", &other),
        rsp(3_501_000, 200, "INVITE", "tb", &other),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].anchor, 2);
    let Evidence::RungDiverged { rung, copies, divergent, .. } = violated(&f[0]) else { panic!() };
    assert_eq!((*rung, *copies, *divergent), (2, 4, 2));
}

/// Another call's, another hop's or another taker's copy is another
/// message: the identity folds none of them together.
#[test]
fn another_call_hop_or_taker_is_another_message() {
    let first = ok_200(&ROWS);
    let other = first.replace("Content-Length: 0", "Content-Length:0");
    let mut on_c2 = rsp(501_000, 200, "INVITE", "tb", &other);
    on_c2.call_id = "c2".to_string();
    let mut on_hop1 = rsp(501_000, 200, "INVITE", "tb", &other);
    on_hop1.hop = 1;
    let mut to_other = rsp(501_000, 200, "INVITE", "tb", &other);
    to_other.dst = "10.0.0.3:5060".to_string();
    let f = eval(&[rsp(1_000, 200, "INVITE", "tb", &first), on_c2, on_hop1, to_other]);
    assert!(f.is_empty(), "{f:?}");
}

// ── the one masked row: the emitter's own Record-Route ──────────────────

const PROXY: &str = "10.0.0.9:5080";
const WORKER: &str = "10.0.0.2:5091";

/// The a-leg INVITE a proxy forwards to a worker: its own Via on top, then
/// `rows` (its Record-Route stamps), then the caller's rows.
fn forwarded_invite(rows: &[&str]) -> String {
    format!(
        "INVITE sip:b@10.0.0.2:5091 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5080;branch=z9hG4bK-p1;rport\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1;received=10.0.0.1;rport=5060\r\n\
             {}\r\n\
             From: <sip:a@h>;tag=fa\r\n\
             To: <sip:b@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 69\r\n\
             Content-Length: 0\r\n\r\n",
        rows.join("\r\n")
    )
}

/// A copy the proxy put on the wire toward the worker.
fn forwarded(at_us: u64, raw: &str) -> Msg {
    let mut m = req(at_us, "INVITE", "z9hG4bK-p1", raw);
    m.src = PROXY.to_string();
    m.dst = WORKER.to_string();
    m
}

const OWN_COOKIE_B2: &str = "Record-Route: <sip:10.0.0.9:5080;w_pri=b1;w_bak=b2;lr>";
const OWN_COOKIE_NONE: &str = "Record-Route: <sip:10.0.0.9:5080;w_pri=b1;w_bak=\"\";lr>";
const OWN_OUTBOUND: &str = "Record-Route: <sip:10.0.0.9:5080;outbound;lr>";
const FOREIGN: &str = "Record-Route: <sip:10.0.0.7:5060;lr>";

#[test]
fn a_proxys_own_record_route_row_may_differ_between_rungs() {
    let first = forwarded_invite(&[OWN_OUTBOUND, OWN_COOKIE_B2, FOREIGN]);
    let rung = forwarded_invite(&[OWN_OUTBOUND, OWN_COOKIE_NONE, FOREIGN]);
    assert_ne!(first, rung);
    let f = eval(&[forwarded(1_000, &first), forwarded(501_000, &rung)]);
    assert_eq!(f.len(), 1, "one occasion, the message: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{f:?}");
    assert_eq!(f[0].anchor, 1);
}

/// A Record-Route naming anyone else is not the emitter's to re-render:
/// the copies are charged, and the finding points into the ORIGINAL bytes
/// past the masked row, whose length differs between the two.
#[test]
fn a_foreign_record_route_row_on_a_rung_is_violated() {
    let first = forwarded_invite(&[OWN_COOKIE_B2, FOREIGN]);
    let rung = forwarded_invite(&[OWN_COOKIE_NONE, "Record-Route: <sip:10.0.0.7:5060;x=1;lr>"]);
    let f = eval(&[forwarded(1_000, &first), forwarded(501_000, &rung)]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, PROXY);
    let Evidence::RungDiverged { region, offset, first_line, rung_line, .. } = violated(&f[0])
    else {
        panic!()
    };
    assert_eq!(region, "head");
    assert_eq!(
        *offset,
        first.find(FOREIGN).unwrap() + "Record-Route: <sip:10.0.0.7:5060;".len(),
        "the offset is into the first copy, past its own (longer) cookie row"
    );
    assert_eq!(first_line.as_deref(), Some(FOREIGN));
    assert_eq!(rung_line.as_deref(), Some("Record-Route: <sip:10.0.0.7:5060;x=1;lr>"));

    // The same row is nobody's to re-render at another vantage: the worker
    // re-sending the request it took would be charged for the cookie row.
    let mut a = forwarded(1_000, &forwarded_invite(&[OWN_COOKIE_B2]));
    let mut b = forwarded(501_000, &forwarded_invite(&[OWN_COOKIE_NONE]));
    (a.src, b.src) = (WORKER.to_string(), WORKER.to_string());
    let f = eval(&[a, b]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{f:?}");
}

/// Every other byte stays in the comparison: a `CSeq` re-rendered beside a
/// re-stamped cookie is charged, and located in the `CSeq` row.
#[test]
fn a_re_rendered_row_beside_the_masked_one_is_still_violated() {
    let first = forwarded_invite(&[OWN_COOKIE_B2]);
    let rung = forwarded_invite(&[OWN_COOKIE_NONE]).replace("CSeq: 1 INVITE", "CSeq:  1 INVITE");
    let f = eval(&[forwarded(1_000, &first), forwarded(501_000, &rung)]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Evidence::RungDiverged { offset, first_line, rung_line, .. } = violated(&f[0]) else {
        panic!()
    };
    assert_eq!(*offset, first.find("CSeq: 1").unwrap() + "CSeq: ".len());
    assert_eq!(first_line.as_deref(), Some("CSeq: 1 INVITE"));
    assert_eq!(rung_line.as_deref(), Some("CSeq:  1 INVITE"));
}

/// The mask pairs rows, it does not license their absence: a rung that
/// drops the emitter's own row, or adds one, is charged at that row — in
/// the copy that carries it, against whatever the other carries there.
#[test]
fn a_dropped_or_added_own_record_route_row_on_a_rung_is_violated() {
    let with = forwarded_invite(&[OWN_OUTBOUND, OWN_COOKIE_B2, FOREIGN]);
    let without = forwarded_invite(&[OWN_OUTBOUND, FOREIGN]);

    let f = eval(&[forwarded(1_000, &with), forwarded(501_000, &without)]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Evidence::RungDiverged { offset, first_line, rung_line, .. } = violated(&f[0]) else {
        panic!()
    };
    assert_eq!(*offset, with.find(OWN_COOKIE_B2).unwrap(), "the dropped row, in the first copy");
    assert_eq!(first_line.as_deref(), Some(OWN_COOKIE_B2));
    assert_eq!(rung_line.as_deref(), Some(FOREIGN));

    let f = eval(&[forwarded(1_000, &without), forwarded(501_000, &with)]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Evidence::RungDiverged { offset, first_line, rung_line, .. } = violated(&f[0]) else {
        panic!()
    };
    assert_eq!(
        *offset,
        without.find(FOREIGN).unwrap(),
        "the added row, where the first copy has none"
    );
    assert_eq!(first_line.as_deref(), Some(FOREIGN));
    assert_eq!(rung_line.as_deref(), Some(OWN_COOKIE_B2));
}

/// A row that folds a foreign hop beside the emitter's own is compared
/// whole — the conservative reading of a row the emitter only half owns.
#[test]
fn a_folded_record_route_row_with_a_foreign_hop_is_compared_whole() {
    let folded = |cookie: &str| format!("{cookie}, <sip:10.0.0.7:5060;lr>");
    let first = forwarded_invite(&[&folded(OWN_COOKIE_B2)]);
    let rung = forwarded_invite(&[&folded(OWN_COOKIE_NONE)]);
    let f = eval(&[forwarded(1_000, &first), forwarded(501_000, &rung)]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Evidence::RungDiverged { first_line, .. } = violated(&f[0]) else { panic!() };
    assert_eq!(first_line.as_deref(), Some(folded(OWN_COOKIE_B2).as_str()));
}
