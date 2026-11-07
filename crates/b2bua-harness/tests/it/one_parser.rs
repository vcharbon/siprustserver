//! One process reads the wire one way, on the fake clock: a datagram reads the
//! same whichever stage parses it, the transaction layer below the brake's
//! threshold, the brake at it, and the brake below it while a refused INVITE is
//! remembered. `b2bua-runner-kit`'s `parser_wiring` proves the same through the
//! runner's own binds, on real sockets.

use std::time::Duration;

use b2bua::ingress_brake::IngressBrakeConfig;
use b2bua_harness::B2buaSut;
use scenario_harness::{Agent, Harness};
use sip_message::{SipMessage, SipParserLimits};

const QUEUE_MAX: usize = 64;
/// The queue depth forced to stand past the brake's threshold.
const ABOVE: usize = 60;
/// The most lines the process reads.
const READS: usize = 12;
const BOB: &str = "127.0.0.1:5070";

/// An OPTIONS of `lines` lines in all, from `from`.
fn options(from: &Agent, lines: usize, id: &str) -> Vec<u8> {
    let own = [
        "OPTIONS sip:worker@127.0.0.1 SIP/2.0".to_string(),
        format!("Via: SIP/2.0/UDP {};branch=z9hG4bK-{id}", from.addr()),
        "Max-Forwards: 70".to_string(),
        "From: <sip:alice@127.0.0.1>;tag=a1".to_string(),
        "To: <sip:worker@127.0.0.1>".to_string(),
        format!("Call-ID: {id}@127.0.0.1"),
        "CSeq: 1 OPTIONS".to_string(),
        "Content-Length: 0".to_string(),
    ];
    let pad = std::iter::repeat_n("X-Pad: p".to_string(), lines - own.len());
    let all: Vec<String> = own.into_iter().chain(pad).collect();
    format!("{}\r\n\r\n", all.join("\r\n")).into_bytes()
}

/// The status of what `sut` answers an OPTIONS of `lines` lines with.
async fn answer(h: &Harness, alice: &Agent, sut: &B2buaSut, lines: usize, id: &str) -> Option<u16> {
    alice.try_send_datagram(&options(alice, lines, id), sut.addr).await.expect("sent");
    h.advance(Duration::from_millis(500)).await;
    let mut status = None;
    while let Some(msg) = alice.take_queued().await {
        if let SipMessage::Response(r) = msg {
            status = Some(r.status());
        }
    }
    status
}

/// The same pair through each stage: at the limit it answers `within`, past it
/// it answers `past`.
async fn reads_the_same(name: &str, limits: SipParserLimits, past: Option<u16>) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", BOB).await;
    let sut = B2buaSut::route_all_to("127.0.0.1", 5070)
        .ingress_brake(IngressBrakeConfig { queue_max: QUEUE_MAX, threshold_pct: 50 })
        .tune(move |config| config.parser_limits = limits)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    assert_eq!(answer(&h, &alice, &sut, READS, "below-within").await, Some(200), "below");
    assert_eq!(answer(&h, &alice, &sut, READS + 1, "below-past").await, past, "below");

    sut.force_brake_depth(Some(ABOVE));
    assert_eq!(answer(&h, &alice, &sut, READS, "above-within").await, Some(200), "above");
    assert_eq!(answer(&h, &alice, &sut, READS + 1, "above-past").await, past, "above");

    // A normal INVITE refused at the brake: a refusal is now remembered.
    let mut refused = alice.invite(&bob).through(sut.addr).send().await;
    refused.expect(503).await;
    sut.force_brake_depth(None);
    assert_eq!(answer(&h, &alice, &sut, READS, "memo-within").await, Some(200), "memo");
    assert_eq!(answer(&h, &alice, &sut, READS + 1, "memo-past").await, past, "memo");

    sut.assert_fully_reaped();
    h.finish().await;
}

/// Past the limit the process reads, nothing answers at any stage.
#[tokio::test(start_paused = true)]
async fn past_the_read_limit_nothing_answers_at_any_stage() {
    let limits = SipParserLimits { max_header_lines: READS, ..SipParserLimits::default() };
    reads_the_same("one-parser-read-limit", limits, None).await;
}

/// Past a stated first-ingress bound, a 400 answers at every stage.
#[tokio::test(start_paused = true)]
async fn past_the_first_ingress_bound_a_400_answers_at_every_stage() {
    let mut limits = SipParserLimits::default();
    limits.first_ingress.header_lines = READS;
    reads_the_same("one-parser-bound", limits, Some(400)).await;
}
