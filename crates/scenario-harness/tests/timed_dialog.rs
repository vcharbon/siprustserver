//! Virtual-time dialog — exercises `Harness::advance` under a **paused** tokio
//! runtime (`start_paused = true`).
//!
//! Scenario the user asked for: bob answers slowly, then the call stays up a
//! while before teardown.
//!
//! ```text
//!   T+0s    alice ──INVITE──▶ bob
//!   T+0s    alice ◀──180──── bob          (ringing)
//!   …wait 10 simulated seconds (bob "rings")…
//!   T+10s   alice ◀──200──── bob          (answer)
//!   T+10s   alice ──ACK────▶ bob          (call connected)
//!   …call connected for 20 simulated seconds…
//!   T+30s   alice ──BYE────▶ bob
//!   T+30s   alice ◀──200──── bob
//! ```
//!
//! Because the recorder's `at_ms` rides the same monotonic clock
//! `tokio::time::advance` drives (sip-clock seam), the 10 s / 20 s gaps appear
//! in the report timestamps. The fabric also applies the default 100 ms
//! one-hop transit delay, so every message is *received* 100 ms after it is
//! *sent* (`received_ms == sent_ms + 100`), and parking on each `recv`
//! auto-advances virtual time by that 100 ms.

use scenario_harness::Harness;
use std::time::Duration;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

#[tokio::test(start_paused = true)]
async fn slow_answer_then_long_call() {
    let h = Harness::new("timed-call").describe(
        "bob rings for 10s before answering, call stays connected for 20s, \
         then alice hangs up. Drives virtual time with Harness::advance.",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // INVITE / 180 at T+0.
    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // bob rings for 10 simulated seconds before answering.
    h.advance(Duration::from_secs(10)).await;

    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // call connected for 20 simulated seconds.
    h.advance(Duration::from_secs(20)).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // --- assert the virtual timeline landed in the recording -----------------
    let report = h.finish().await;
    let entries = report.entries();
    assert_eq!(entries.len(), 6);
    assert!(entries.iter().all(|e| e.delivered));

    // Every hop shows the fabric's 100 ms transit delay.
    for e in &entries {
        assert_eq!(
            e.received_ms,
            Some(e.sent_ms + scenario_harness::SIMULATED_TRANSIT_DELAY_MS),
            "each message is received one transit delay after it is sent"
        );
    }

    // The advances open the gaps: ~10 s of ringing between 180 and 200, ~20 s
    // of connected call between ACK and BYE. (Exact values carry an extra
    // 100 ms per intervening hop from the transit-delay auto-advance, so assert
    // the floor rather than a brittle exact number.)
    let sent: Vec<u64> = entries.iter().map(|e| e.sent_ms).collect();
    assert!(sent[2] - sent[1] >= 10_000, "≥10s ring before answer (got {})", sent[2] - sent[1]);
    assert!(sent[4] - sent[3] >= 20_000, "≥20s connected before BYE (got {})", sent[4] - sent[3]);

    // The report renders the transit delay (two-timestamp form) and the gaps.
    let global = std::fs::read_to_string({
        let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("timed-call");
        let _ = std::fs::remove_dir_all(&out);
        scenario_harness::report::write_all(&report, &out).unwrap();
        out.join("timed-call.global.txt")
    })
    .unwrap();
    // The unified global view carries the transit (sent → rcvd) on each message
    // that crossed with a delay, plus the relative send stamps.
    assert!(global.contains("sent T+") && global.contains("→ rcvd T+"), "transit delay shown");
    assert!(global.contains("T+10."), "200 OK stamped ~T+10s");
    assert!(global.contains("T+30."), "BYE stamped ~T+30s");
}
