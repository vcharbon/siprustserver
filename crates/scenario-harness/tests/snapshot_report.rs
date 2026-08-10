//! `Harness::snapshot_report` renders the full artifact set from a LIVE
//! harness — `report::write_all` on the snapshot, then the run still closes
//! through the audited `finish()`. (Env-var-free on purpose: the Drop-guard
//! env-gate tests live in the single-test `artifact_on_drop` binary.)

use std::fs;
use std::path::PathBuf;

use scenario_harness::Harness;

const SDP_OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// A live `snapshot_report()` mid-session renders the PASS artifacts without
/// consuming the harness; the run then terminates properly through `finish()`
/// (RFC hard gate + recording close), as every callflow test must.
#[tokio::test(start_paused = true)]
async fn snapshot_report_renders_artifacts_without_consuming_the_harness() {
    let h = Harness::new("snapshot-before-finish").describe(
        "Full INVITE/180/200/ACK/BYE/200 dialog; the artifacts are rendered \
         from a live snapshot taken before finish().",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(SDP_OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(SDP_ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // Snapshot + render while the harness is still live.
    let snapshot = h.snapshot_report();
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("snapshot-before-finish");
    let _ = fs::remove_dir_all(&out);
    let written = scenario_harness::report::write_all(&snapshot, &out).expect("write artifacts");
    assert!(!written.is_empty());
    assert!(out.join("snapshot-before-finish.svg").exists());
    assert!(out.join("snapshot-before-finish.html").exists());
    let global = fs::read_to_string(out.join("snapshot-before-finish.global.txt")).unwrap();
    assert!(global.contains("Status: PASS"), "clean live snapshot renders PASS:\n{global}");
    assert!(global.contains("INVITE sip:bob@127.0.0.1:5070 SIP/2.0"), "{global}");
    assert!(global.contains("BYE sip:bob@127.0.0.1:5070 SIP/2.0"), "{global}");

    // The harness was not consumed: the call terminated (BYE/200 above) and
    // the run still closes through the audited finish().
    let report = h.finish().await;
    assert_eq!(report.entries().len(), 6, "the whole dialog was recorded");
}
