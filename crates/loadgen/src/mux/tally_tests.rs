use super::*;

fn req(method: &str, cseq: u32) -> Sniffed {
    Sniffed::of(
        format!(
            "{method} sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK{cseq}\r\n\
             Call-ID: c1\r\nCSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
}

fn resp(status: u16, method: &str, cseq: u32, rseq: Option<u32>) -> Sniffed {
    let rseq = rseq.map_or(String::new(), |r| format!("Require: 100rel\r\nRSeq: {r}\r\n"));
    Sniffed::of(
        format!(
            "SIP/2.0 {status} Reason\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK{cseq}\r\n\
             Call-ID: c1\r\nCSeq: {cseq} {method}\r\n{rseq}Content-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
}

fn arrows(m: &CallMessages) -> Vec<(String, String, String, u32, u64, u64, u64)> {
    m.arrows
        .iter()
        .map(|(k, s)| {
            (k.from.clone(), k.to.clone(), k.label.clone(), k.nth, s.count, s.duplicates, s.resent)
        })
        .collect()
}

fn a(
    from: &str,
    to: &str,
    label: &str,
    nth: u32,
    dups: u64,
    resent: u64,
) -> (String, String, String, u32, u64, u64, u64) {
    (from.into(), to.into(), label.into(), nth, 1, dups, resent)
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_copy_is_a_retransmission_and_a_new_transaction_a_new_occurrence() {
    let t = CallTally::new("alice", true);
    let now = Instant::now();
    t.on_datagram(None, Dir::Out, &req("INVITE", 1), now);
    t.on_datagram(None, Dir::Out, &req("INVITE", 1), now);
    t.on_resend(None, &req("INVITE", 1));
    t.on_datagram(None, Dir::In, &resp(183, "INVITE", 1, Some(1)), now);
    t.on_datagram(None, Dir::In, &resp(183, "INVITE", 1, Some(1)), now);
    t.on_datagram(None, Dir::In, &resp(183, "INVITE", 1, Some(2)), now);
    t.on_datagram(None, Dir::In, &resp(200, "INVITE", 1, None), now);
    t.on_datagram(None, Dir::Out, &req("INVITE", 2), now);
    t.on_datagram(None, Dir::In, &resp(200, "INVITE", 2, None), now);
    t.on_datagram(Some("bob"), Dir::In, &req("BYE", 1), now);
    t.on_datagram(Some("bob"), Dir::Out, &resp(200, "BYE", 1, None), now);
    assert_eq!(
        arrows(&t.finish()),
        vec![
            a("alice", "SUT", "INVITE", 1, 0, 2),
            a("SUT", "alice", "183 (INVITE)", 1, 1, 0),
            a("SUT", "alice", "183 (INVITE)", 2, 0, 0),
            a("SUT", "alice", "200 (INVITE)", 1, 0, 0),
            a("alice", "SUT", "INVITE", 2, 0, 0),
            a("SUT", "alice", "200 (INVITE)", 2, 0, 0),
            a("SUT", "bob", "BYE", 1, 0, 0),
            a("bob", "SUT", "200 (BYE)", 1, 0, 0),
        ],
        "a re-INVITE and a 183 with a new RSeq are new occurrences"
    );
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn the_error_goes_to_the_last_inbound_arrow_before_the_outcome() {
    let t = CallTally::new("alice", false);
    t.mark_outcome();
    assert_eq!(t.finish().charge, None, "nothing came in before the outcome");

    let t = CallTally::new("alice", false);
    let now = Instant::now();
    t.on_datagram(None, Dir::Out, &req("INVITE", 1), now);
    t.on_datagram(None, Dir::In, &resp(180, "INVITE", 1, None), now);
    t.on_datagram(None, Dir::Out, &req("CANCEL", 1), now);
    t.mark_outcome();
    t.on_datagram(None, Dir::In, &resp(200, "CANCEL", 1, None), now);
    t.mark_outcome();
    let m = t.finish();
    assert_eq!(m.charge.map(|k| k.label), Some("180 (INVITE)".to_string()), "teardown is after");
    assert_eq!(m.arrows.len(), 4, "teardown traffic still counted");
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn an_arrow_keeps_its_offset_from_call_start() {
    let t = CallTally::new("alice", false);
    tokio::time::advance(std::time::Duration::from_millis(250)).await;
    t.on_datagram(None, Dir::Out, &req("INVITE", 1), Instant::now());
    let m = t.finish();
    assert_eq!(m.arrows[0].1.offset_ms_sum, 250.0);
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_call_keeps_at_most_its_cap_of_arrows() {
    let t = CallTally::new("alice", false);
    let now = Instant::now();
    for cseq in 0..MAX_ARROWS as u32 + 10 {
        t.on_datagram(None, Dir::Out, &req("OPTIONS", cseq), now);
    }
    assert_eq!(t.finish().arrows.len(), MAX_ARROWS);
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_datagram_after_the_call_is_folded_is_not_tallied() {
    let t = CallTally::new("alice", true);
    let now = Instant::now();
    t.on_datagram(None, Dir::Out, &req("INVITE", 1), now);
    t.on_datagram(None, Dir::In, &resp(200, "INVITE", 1, None), now);
    assert_eq!(t.finish().arrows.len(), 2);
    t.on_resend(None, &req("INVITE", 1));
    t.on_datagram(None, Dir::In, &resp(200, "INVITE", 1, None), now);
    t.on_datagram(None, Dir::Out, &req("BYE", 2), now);
    assert!(t.finish().arrows.is_empty(), "the late copies reach no arrow");
}
