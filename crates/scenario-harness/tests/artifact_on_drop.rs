//! The Drop-path artifact property: a run that never reaches `finish()` still
//! leaves its full SVG/HTML/text ladders under `SCENARIO_ARTIFACT_DIR` (unset
//! ⇒ nothing is written), FAIL-bannered and carrying the panic message on the
//! panic path or `finish() never reached` on a clean drop.
//!
//! This binary runs exactly ONE test: `set_var`/`remove_var` are only unsound
//! while another thread may be inside `getenv`, and a single-test binary has
//! no such thread. Keep it single-test — env-free harness tests go elsewhere
//! (e.g. `snapshot_report.rs`).

use std::fs;
use std::path::PathBuf;

use scenario_harness::Harness;

const SDP_OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// Drive a short callflow to mid-dialog and then panic, unwinding past
/// `finish()`. The properly-terminated rule does not bind here: the SUBJECT of
/// this test is the mid-flight failure dump itself — the harness is dropped by
/// the unwind, no SUT holds call state, and the recording is all that remains.
fn run_panicking_scenario(name: &'static str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("build paused runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(async move {
            let h = Harness::new(name);
            let alice = h.agent("alice", "127.0.0.1:5060").await;
            let bob = h.agent("bob", "127.0.0.1:5070").await;
            let mut call = alice.invite(&bob).with_sdp(SDP_OFFER).send().await;
            let mut uas = bob.receive("INVITE").await;
            uas.respond(180, "Ringing").await;
            call.expect(180).await;
            panic!("boom: the 200 OK never came");
        })
    }));
    assert!(result.is_err(), "the scenario must unwind");
}

/// Drive a COMPLETE, properly-terminated dialog (INVITE/180/200/ACK/BYE/200)
/// and then drop the harness without calling `finish()` — the forgotten-finish
/// branch of the guard. The trace is RFC-clean, so the `CseqGate` Drop
/// backstop passes; only the artifact writer has something to say.
fn run_clean_drop_scenario(name: &'static str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("build paused runtime");
    rt.block_on(async move {
        let h = Harness::new(name);
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
        // Deliberately NO finish(): the drop below is the subject.
        drop(h);
    });
}

/// One test owns the env var end to end: unset ⇒ the Drop guard writes
/// nothing; set ⇒ a panic unwind leaves the full artifact set FAIL-bannered
/// with the panic message, and a clean drop without `finish()` leaves the same
/// set stamped `finish() never reached`. The stderr `PanicDump` path still
/// runs first (this test completing at all proves no double-panic abort on
/// Drop).
#[test]
fn drop_path_writes_fail_artifacts_only_when_the_env_gate_is_set() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("artifact-on-drop");
    let _ = fs::remove_dir_all(&root);

    // Leading half: env unset ⇒ off (CI stays artifact-free).
    std::env::remove_var("SCENARIO_ARTIFACT_DIR");
    run_panicking_scenario("artifact-env-unset");
    assert!(!root.exists(), "no artifacts may be written while the env gate is unset");

    std::env::set_var("SCENARIO_ARTIFACT_DIR", &root);
    run_panicking_scenario("artifact-panic-path");
    run_clean_drop_scenario("artifact-clean-drop");
    std::env::remove_var("SCENARIO_ARTIFACT_DIR");

    let case = root.join("artifact-panic-path");
    assert!(case.join("artifact-panic-path.svg").exists(), "svg written");
    assert!(case.join("ext/alice.txt").exists(), "per-endpoint ladder written");
    assert!(
        !root.join("artifact-env-unset").exists(),
        "the env-unset run left nothing behind"
    );

    let global = fs::read_to_string(case.join("artifact-panic-path.global.txt")).unwrap();
    assert!(global.contains("Status: FAIL"), "banner reads FAIL, not vacuous PASS:\n{global}");
    assert!(
        global.contains("boom: the 200 OK never came"),
        "the panic message reaches the text artifact:\n{global}"
    );
    assert!(global.contains("INVITE"), "the wire trace is in the ladder:\n{global}");

    let html = fs::read_to_string(case.join("artifact-panic-path.html")).unwrap();
    assert!(html.contains("FAIL"), "html banner reads FAIL");
    assert!(
        html.contains("boom: the 200 OK never came"),
        "the panic message reaches the html artifact"
    );

    // Clean drop without finish(): same artifact set, FAIL-bannered with the
    // forgotten-finish note instead of a panic message.
    let clean = root.join("artifact-clean-drop");
    assert!(clean.join("artifact-clean-drop.svg").exists(), "svg written");
    let global = fs::read_to_string(clean.join("artifact-clean-drop.global.txt")).unwrap();
    assert!(global.contains("Status: FAIL"), "a finish-less run renders FAIL:\n{global}");
    assert!(
        global.contains("finish() never reached"),
        "the clean-drop artifact states why it failed:\n{global}"
    );
    assert!(global.contains("BYE"), "the terminated dialog is in the ladder:\n{global}");
}
