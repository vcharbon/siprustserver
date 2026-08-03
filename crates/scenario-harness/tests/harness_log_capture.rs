//! The session's log/trace capture (ADR-0026 §7): a live [`Harness`] holds the
//! thread-scoped `observe` buffer, so whatever the SUT logs and traces during a
//! run is there for the panic dump to print — and `finish()` hands the thread
//! back untouched.
//!
//! No wire traffic: the subject is the capture wiring of the session itself, so
//! there is no call to establish or terminate and the RFC hard gate at
//! `finish()` judges an empty trace as it does every run. What the SUT *emits*
//! is never a scenario oracle (the `Recorder` is); this test is one of the
//! dedicated tests OF the trace machinery that may read the buffer.

use scenario_harness::Harness;

#[tokio::test(start_paused = true)]
async fn a_running_harness_captures_what_the_sut_logs() {
    assert!(
        observe::current_test_buffer().is_none(),
        "nothing captures on this thread before a harness binds",
    );

    let h = Harness::new("log-capture")
        .describe("A live harness owns the thread's log/trace capture buffer");
    let log = observe::current_test_buffer().expect("a harness installs the capture buffer");

    tracing::info!(node = "w-0", "takeover complete");

    let captured = log.matching("takeover complete");
    assert_eq!(captured.len(), 1, "the run's lifecycle line lands in the buffer: {:?}", log.lines());
    assert!(captured[0].contains("node=w-0"), "with its fields: {}", captured[0].line());

    h.finish().await;

    assert!(
        observe::current_test_buffer().is_none(),
        "the session's end releases the capture buffer",
    );
}
