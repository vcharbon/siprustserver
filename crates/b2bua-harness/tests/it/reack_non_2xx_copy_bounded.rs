//! RFC 3261 §17.1.1.3 — a non-2xx INVITE final is ACKed hop by hop, and its
//! answerer re-sends it on Timer G's schedule only (§17.2.1: T1 doubling,
//! capped at T2, giving up at 64·T1), so a copy beyond that schedule's count,
//! arriving within T1 of the last re-ACK, is not a retransmission the UAC owes
//! an ACK (ADR-0063 §2).
//!
//! The callee answers the call, then sends a 488 to an INVITE one CSeq above
//! the one the B2BUA sent it, on a branch no transaction here holds: evidence
//! the record cannot explain (ADR-0055 §4, trigger 1). The B2BUA ACKs it, keeps
//! that ACK and ends the call. The callee is broken on purpose: it re-sends the
//! 488 on every ACK it receives, while it holds the B2BUA's BYE unanswered, so
//! the call is terminating throughout. The B2BUA re-ACKs the copies a compliant
//! answerer can send and nothing else, the ping-pong stops, and the call ends
//! with the callee's 200 to the BYE, one CDR, fully reaped.
//!
//! Each round sends one copy, then reads after a settled advance of a few
//! transits: the verdict depends on no in-order draining of the paused runtime.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness, Inbound, ServerTxn, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// One-hop transit: the ping-pong runs at wire speed, far inside 64·T1.
const TRANSIT_MS: u64 = 1;
/// The settled advance after each copy: the copy's hop, the re-ACK's hop, and
/// slack for the SUT's turn.
const ROUND_MS: u64 = 5;
/// The test's own bound on the ping-pong: a SUT that never stops re-ACKing
/// ends the loop here, never the wall clock.
const ROUNDS: usize = 1000;
/// The most copies of one non-2xx final a compliant answerer sends after the
/// first (RFC 3261 §17.2.1, Timer G, T1 = 500 ms, T2 = 4 s): at 0.5, 1.5, 3.5,
/// 7.5, 11.5, 15.5, 19.5, 23.5, 27.5 and 31.5 s, the next falling past Timer H
/// = 64·T1 = 32 s.
const COMPLIANT_COPIES: usize = 10;
/// The branch of the 488, which no client transaction of the B2BUA holds.
const EVIDENCE_BRANCH: &str = "z9hG4bK-evidence-488";

/// `two_hundred`, the callee's 200 to the INVITE the B2BUA sent it, rewritten
/// into a 488 to an INVITE one CSeq above, on [`EVIDENCE_BRANCH`], bodyless.
fn evidence_488(two_hundred: &str) -> Vec<u8> {
    let head = two_hundred.split("\r\n\r\n").next().unwrap_or_default();
    let mut lines = vec!["SIP/2.0 488 Not Acceptable Here".to_string()];
    for line in head.split("\r\n").skip(1) {
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        match name.trim().to_ascii_lowercase().as_str() {
            "via" | "v" => lines.push(format!("{name}:{}", with_branch(value))),
            "cseq" => lines.push(format!("{name}: {} INVITE", above(value))),
            "content-type" | "c" | "content-length" | "l" => {}
            _ => lines.push(line.to_string()),
        }
    }
    lines.push("Content-Length: 0".into());
    format!("{}\r\n\r\n", lines.join("\r\n")).into_bytes()
}

/// `via`, a Via header value, with its branch replaced by [`EVIDENCE_BRANCH`].
fn with_branch(via: &str) -> String {
    via.split(';')
        .map(|p| {
            if p.trim().to_ascii_lowercase().starts_with("branch=") {
                format!("branch={EVIDENCE_BRANCH}")
            } else {
                p.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// The CSeq number one above the one `cseq`, a CSeq header value, names.
fn above(cseq: &str) -> u32 {
    let seq: u32 = cseq.split_whitespace().next().and_then(|n| n.parse().ok()).expect("a CSeq");
    seq + 1
}

/// The last 200 to an INVITE `from` sent `to` on the recorded wire.
fn last_invite_200(h: &Harness, from: SocketAddr, to: SocketAddr) -> String {
    h.wire_entries()
        .into_iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(b"SIP/2.0 200"))
        .map(|e| String::from_utf8_lossy(&e.raw).to_string())
        .rfind(|text| text.contains(" INVITE\r\n"))
        .expect("the callee's 200")
}

/// The ACKs the SUT at `b2bua` sent to `to` in the wire entries past `from`.
fn acks_to(h: &Harness, b2bua: SocketAddr, to: SocketAddr, from: usize) -> usize {
    h.wire_entries()
        .into_iter()
        .skip(from)
        .filter(|e| e.from == b2bua && e.to == to && e.raw.starts_with(b"ACK "))
        .count()
}

/// What `callee` holds queued: the ACKs it counted and the BYE, when one came.
async fn take_acks_and_bye(callee: &Agent) -> (usize, Option<ServerTxn>) {
    let (mut acks, mut bye) = (0, None);
    while let Some(inbound) = callee.take_queued_inbound().await {
        let Inbound::Request(txn) = inbound else { continue };
        match txn.request().method().as_str() {
            "ACK" => acks += 1,
            "BYE" => bye = Some(*txn),
            other => panic!("the callee hears only an ACK and a BYE, got {other}"),
        }
    }
    (acks, bye)
}

/// The findings of the round no INVITE opened, the stated target: the callee's
/// 488 answers no request it received (RFC 3261 §8.1.3.3), so the ACK the B2BUA
/// owes it names that 488's CSeq (§17.1.1.3) and the B2BUA's BYE is numbered
/// past it, never re-using a number the callee saw (§12.2.1.1).
fn waive_the_phantom_round(h: &Harness) {
    let why = "the callee answers an INVITE it never received";
    for rule in ["response-correlation", "response-cseq-matches-transaction"] {
        h.waive(WaiverScope::rule(rule, why).on_party("bob"));
    }
    for rule in ["ack-cseq-matches-invite", "cseq-in-dialog-order"] {
        h.waive(WaiverScope::rule(rule, why).on_party("b2bua"));
    }
    h.waive(WaiverScope::rule("dialog-cseq-skip", why).on_party("bob"));
}

/// What the SUT put on the wire while the callee repeated its 488.
struct PingPong {
    /// The copies of the 488 the callee sent.
    copies: usize,
    /// The ACKs the SUT sent the callee.
    re_acks: usize,
}

/// Plays the broken callee: re-sends `evidence` from `callee` to the SUT at
/// `b2bua` and waits one round, again while that round drew an ACK, for at
/// most [`ROUNDS`] copies.
async fn ping_pong(h: &Harness, b2bua: SocketAddr, callee: &Agent, evidence: &[u8]) -> PingPong {
    let before = h.wire_entries().len();
    let mut copies = 0;
    for _ in 0..ROUNDS {
        let round = h.wire_entries().len();
        callee.try_send_datagram(evidence, b2bua).await.expect("the copy leaves");
        copies += 1;
        h.advance(Duration::from_millis(ROUND_MS)).await;
        callee.drain().await;
        if acks_to(h, b2bua, callee.addr(), round) == 0 {
            break;
        }
    }
    PingPong { copies, re_acks: acks_to(h, b2bua, callee.addr(), before) }
}

/// The callee's 488 beyond the record, re-sent on every ACK while the call
/// terminates: the B2BUA re-ACKs the ten copies a compliant answerer sends and
/// absorbs the eleventh, which arrives within T1 of the last re-ACK.
#[tokio::test(start_paused = true)]
async fn a_non_2xx_repeated_on_every_ack_draws_bounded_re_acks_while_terminating() {
    let h = Harness::with_transit_delay("b2bua-reack-bounded-non-2xx", TRANSIT_MS);
    waive_the_phantom_round(&h);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob.addr().port())
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &at)
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    call.ack().await;
    bob.receive("ACK").await;

    let evidence = evidence_488(&last_invite_200(&h, bob.addr(), b2bua.addr));
    bob.try_send_datagram(&evidence, b2bua.addr).await.expect("the 488 leaves");
    h.advance(Duration::from_millis(ROUND_MS)).await;
    let (acked, bye) = take_acks_and_bye(&bob).await;
    assert_eq!(acked, 1, "the 488 beyond the record is ACKed (RFC 3261 §17.1.1.3)");
    let mut bye = bye.expect("the B2BUA ends the call on evidence: the callee's BYE");
    alice.receive("BYE").await.respond(200, "OK").await;

    let played = ping_pong(&h, b2bua.addr, &bob, &evidence).await;
    assert_eq!(
        played.re_acks, COMPLIANT_COPIES,
        "the SUT re-ACKs the {COMPLIANT_COPIES} copies a compliant answerer sends (RFC 3261 \
         §17.2.1), got {} re-ACKs over {} copies",
        played.re_acks, played.copies,
    );
    assert_eq!(played.copies, COMPLIANT_COPIES + 1, "the first copy left un-ACKed ends it");

    bye.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
}
