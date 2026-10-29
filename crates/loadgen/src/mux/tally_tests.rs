use sip_message::sniff::identity;

use super::*;

fn req(method: &str, cseq: u32) -> Vec<u8> {
    format!(
        "{method} sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK{cseq}\r\n\
         Call-ID: c1\r\nCSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn resp_tagged(status: u16, method: &str, cseq: u32, rseq: Option<u32>, tag: &str) -> Vec<u8> {
    let rseq = rseq.map_or(String::new(), |r| format!("Require: 100rel\r\nRSeq: {r}\r\n"));
    format!(
        "SIP/2.0 {status} Reason\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK{cseq}\r\n\
         To: <sip:bob@192.0.2.2>;tag={tag}\r\nCall-ID: c1\r\nCSeq: {cseq} {method}\r\n{rseq}\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn resp(status: u16, method: &str, cseq: u32, rseq: Option<u32>) -> Vec<u8> {
    resp_tagged(status, method, cseq, rseq, "t1")
}

fn send(t: &CallTally, leg: Option<&str>, dir: Dir, raw: &[u8]) {
    t.on_datagram(leg, dir, &identity(raw), Instant::now());
}

type Row = (String, String, String, u32, u64, u64, u64);

fn arrows(m: &CallMessages) -> Vec<Row> {
    m.arrows
        .iter()
        .map(|(k, s)| {
            (k.from.clone(), k.to.clone(), k.label.clone(), k.nth, s.count, s.duplicates, s.resent)
        })
        .collect()
}

fn a(from: &str, to: &str, label: &str, nth: u32, dups: u64, resent: u64) -> Row {
    (from.into(), to.into(), label.into(), nth, 1, dups, resent)
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_copy_is_a_retransmission_and_a_new_transaction_a_new_occurrence() {
    let t = CallTally::new("alice");
    send(&t, None, Dir::Out, &req("INVITE", 1));
    send(&t, None, Dir::Out, &req("INVITE", 1));
    t.on_resend(None, &identity(&req("INVITE", 1)));
    send(&t, None, Dir::In, &resp(180, "INVITE", 1, None));
    send(&t, None, Dir::In, &resp(180, "INVITE", 1, None));
    send(&t, None, Dir::In, &resp(183, "INVITE", 1, Some(1)));
    send(&t, None, Dir::In, &resp(183, "INVITE", 1, Some(1)));
    send(&t, None, Dir::In, &resp(183, "INVITE", 1, Some(2)));
    send(&t, None, Dir::In, &resp(200, "INVITE", 1, None));
    send(&t, None, Dir::In, &resp_tagged(200, "INVITE", 1, None, "fork2"));
    send(&t, None, Dir::Out, &req("INVITE", 2));
    send(&t, None, Dir::In, &resp(200, "INVITE", 2, None));
    send(&t, Some("bob"), Dir::In, &req("BYE", 1));
    send(&t, Some("bob"), Dir::Out, &resp(200, "BYE", 1, None));
    assert_eq!(
        arrows(&t.finish()),
        vec![
            a("alice", "SUT", "INVITE", 1, 0, 2),
            a("SUT", "alice", "180 (INVITE)", 1, 1, 0),
            a("SUT", "alice", "183 (INVITE)", 1, 1, 0),
            a("SUT", "alice", "183 (INVITE)", 2, 0, 0),
            a("SUT", "alice", "200 (INVITE)", 1, 0, 0),
            a("SUT", "alice", "200 (INVITE)", 2, 0, 0),
            a("alice", "SUT", "INVITE", 2, 0, 0),
            a("SUT", "alice", "200 (INVITE)", 3, 0, 0),
            a("SUT", "bob", "BYE", 1, 0, 0),
            a("bob", "SUT", "200 (BYE)", 1, 0, 0),
        ],
        "a re-sent 180 is a copy; a new RSeq, a forked final (new To-tag) and a re-INVITE are \
         new occurrences"
    );
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn each_lane_keeps_its_last_new_arrow_in_before_the_outcome() {
    let t = CallTally::new("alice");
    t.mark_outcome();
    assert!(t.finish().charge_of("alice").is_none(), "nothing came in before the outcome");

    let t = CallTally::new("alice");
    send(&t, None, Dir::Out, &req("INVITE", 1));
    send(&t, None, Dir::In, &resp(180, "INVITE", 1, None));
    send(&t, Some("bob"), Dir::In, &req("INVITE", 1));
    send(&t, Some("bob"), Dir::Out, &resp(180, "INVITE", 1, None));
    send(&t, Some("bob"), Dir::In, &req("CANCEL", 1));
    send(&t, Some("bob"), Dir::In, &req("INVITE", 1));
    send(&t, None, Dir::In, &resp(183, "INVITE", 1, Some(1)));
    send(&t, None, Dir::In, &resp(180, "INVITE", 1, None));
    t.mark_outcome();
    send(&t, None, Dir::In, &resp(487, "INVITE", 1, None));
    send(&t, Some("bob"), Dir::In, &req("BYE", 2));
    t.mark_outcome();
    let m = t.finish();
    let label = |who: &str| m.charge_of(who).flatten().map(|k| k.label.clone());
    assert_eq!(label("bob").as_deref(), Some("CANCEL"), "a copy does not move the pointer");
    assert_eq!(label("alice").as_deref(), Some("183 (INVITE)"), "teardown traffic is after");
    assert_eq!(m.charge_of("charlie"), None, "an agent with nothing in: before any reply");
    assert_eq!(m.arrows.len(), 8, "teardown traffic still counted");
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn an_arrow_keeps_its_offset_from_call_start() {
    let t = CallTally::new("alice");
    tokio::time::advance(std::time::Duration::from_millis(250)).await;
    send(&t, None, Dir::Out, &req("INVITE", 1));
    assert_eq!(t.finish().arrows[0].1.offset_ms_sum, 250.0);
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn past_its_cap_a_call_counts_on_its_other_arrow() {
    let t = CallTally::new("alice");
    for cseq in 0..MAX_ARROWS as u32 + 10 {
        send(&t, None, Dir::Out, &req("OPTIONS", cseq));
    }
    send(&t, None, Dir::In, &resp(200, "OPTIONS", MAX_ARROWS as u32 + 5, None));
    t.mark_outcome();
    let m = t.finish();
    assert_eq!((m.arrows.len(), m.other.count), (MAX_ARROWS, 11));
    assert_eq!(m.charge_of("alice"), Some(None), "charged to the other arrow");
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_resend_of_a_message_never_seen_opens_nothing() {
    let t = CallTally::new("alice");
    t.on_resend(None, &identity(&req("INVITE", 1)));
    assert!(t.finish().arrows.is_empty());
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_datagram_after_the_call_is_folded_is_not_tallied() {
    let t = CallTally::new("alice");
    send(&t, None, Dir::Out, &req("INVITE", 1));
    send(&t, None, Dir::In, &resp(200, "INVITE", 1, None));
    assert_eq!(t.finish().arrows.len(), 2);
    t.on_resend(None, &identity(&req("INVITE", 1)));
    send(&t, None, Dir::In, &resp(200, "INVITE", 1, None));
    send(&t, None, Dir::Out, &req("BYE", 2));
    let state = t.state.lock().unwrap();
    assert!(state.entries.is_empty() && state.last_inbound.is_empty(), "released and closed");
    drop(state);
    assert!(t.finish().arrows.is_empty(), "the late copies reach no arrow");
}
