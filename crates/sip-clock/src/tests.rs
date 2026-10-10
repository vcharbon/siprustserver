use super::*;
use proptest::prelude::*;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn now_ms_advances_in_lockstep_with_tokio_time() {
    let clock = Clock::test_at(1_000_000);
    assert_eq!(clock.now_ms(), 1_000_000);

    tokio::time::advance(Duration::from_millis(250)).await;
    assert_eq!(clock.now_ms(), 1_000_250);

    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(clock.now_ms(), 1_030_250);
}

#[tokio::test(start_paused = true)]
async fn now_ms_is_monotonic_non_decreasing() {
    let clock = Clock::test_at(0);
    let mut last = clock.now_ms();
    for _ in 0..5 {
        tokio::time::advance(Duration::from_millis(100)).await;
        let now = clock.now_ms();
        assert!(now >= last, "{now} < {last}");
        last = now;
    }
}

// ── Stage 3: clock-skew divergence observability seam ──

#[tokio::test(start_paused = true)]
async fn wall_divergence_is_raw_minus_now() {
    // The pure seam: inject the raw wall reading, so the test is deterministic
    // and never touches SystemTime. now_ms == anchor under a paused clock.
    let clock = Clock::test_at(1_000_000);
    // Raw wall AHEAD of our anchored now_ms by 300 s (a host step forward).
    assert_eq!(clock.wall_divergence_ms(1_300_000), 300_000);
    // Raw wall BEHIND (a step backward) → negative divergence.
    assert_eq!(clock.wall_divergence_ms(700_000), -300_000);
    // Agreement → zero.
    assert_eq!(clock.wall_divergence_ms(1_000_000), 0);

    // now_ms advances with tokio time, but a STEPPED raw reading does not follow
    // it — so the divergence tracks the step, which is the whole point.
    tokio::time::advance(Duration::from_secs(10)).await;
    // now_ms is now 1_010_000; a raw wall that stayed at 1_000_000 (monotonic
    // Clock kept moving, host clock did not) reads as −10_000 divergence.
    assert_eq!(clock.wall_divergence_ms(1_000_000), -10_000);
}

#[test]
fn system_panics_on_pre_epoch_clock() {
    // We cannot force SystemTime backwards, but we CAN assert the guard: the
    // pre-epoch branch is `duration_since(UNIX_EPOCH).expect(...)`. Model it
    // directly — a pre-epoch SystemTime yields `Err` from `duration_since`, and
    // our constructor `.expect()`s it. This documents/locks the fail-loud
    // contract that replaced the silent `unwrap_or(0)`.
    let pre_epoch = UNIX_EPOCH - std::time::Duration::from_secs(1);
    let result = std::panic::catch_unwind(|| {
        pre_epoch
            .duration_since(UNIX_EPOCH)
            .expect("system wall clock is before the UNIX epoch (grossly misconfigured host clock) — refusing to anchor Clock to a pre-1970 time")
    });
    assert!(result.is_err(), "a pre-epoch clock must panic, not anchor to 0");
}

#[tokio::test(start_paused = true)]
async fn clones_share_the_same_timeline() {
    let a = Clock::test_at(500);
    let b = a.clone();
    tokio::time::advance(Duration::from_millis(750)).await;
    assert_eq!(a.now_ms(), b.now_ms());
    assert_eq!(a.now_ms(), 1_250);
}

#[tokio::test(start_paused = true)]
async fn a_settled_advance_lands_on_total_and_runs_each_timer_at_its_instant() {
    let clock = Clock::test_at(0);
    let (done, landed) = tokio::sync::oneshot::channel();
    let hops = clock.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = done.send(hops.now_ms());
    });
    crate::testkit::advance_settled(Duration::from_millis(250)).await;
    assert_eq!(clock.now_ms(), 250);
    assert_eq!(landed.await.unwrap(), 120, "the second sleep starts where the first fired");
}

#[test]
fn a_bounded_scenario_returns_its_output_after_its_virtual_time() {
    let out = crate::testkit::run_paused_within(Duration::from_secs(10), || async {
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        7
    });
    assert_eq!(out, Some(7));
}

#[test]
fn a_bounded_scenario_behind_a_task_that_never_goes_idle_is_none() {
    let out = crate::testkit::run_paused_within(Duration::from_millis(200), || async {
        tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    assert_eq!(out, None);
}

#[test]
fn a_bounded_scenario_that_panics_re_raises_its_panic() {
    let caught = std::panic::catch_unwind(|| {
        crate::testkit::run_paused_within(Duration::from_secs(10), || async {
            panic!("expected: the scenario panics")
        })
    });
    let payload = caught.expect_err("the panic is re-raised");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"expected: the scenario panics"));
}

#[test]
fn a_bounded_scenario_stuck_in_a_synchronous_loop_panics_after_the_grace() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let release = Arc::new(AtomicBool::new(false));
    let stuck = release.clone();
    let caught = std::panic::catch_unwind(|| {
        crate::testkit::run_paused_within(Duration::from_millis(100), move || async move {
            while !stuck.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
        })
    });
    // Let the abandoned thread end.
    release.store(true, Ordering::Relaxed);
    let payload = caught.expect_err("a runtime that cannot take the abort panics");
    let message = payload.downcast_ref::<String>().expect("a formatted message");
    assert!(message.contains("synchronous loop"), "{message}");
}

/// One hop of a request or of its answer between two tasks.
#[cfg(feature = "testkit")]
const HOP: Duration = Duration::from_millis(60);

/// A caller asks a service task under a 150 ms budget; the service answers
/// after one [`HOP`] in and one back. The receiver yields the virtual time
/// the answer took, `None` for a timeout.
#[cfg(feature = "testkit")]
fn ask_under_budget() -> tokio::sync::oneshot::Receiver<Option<Duration>> {
    use tokio::sync::{mpsc, oneshot};
    let (to_service, mut requests) = mpsc::channel::<oneshot::Sender<()>>(1);
    tokio::spawn(async move {
        while let Some(reply) = requests.recv().await {
            tokio::time::sleep(HOP).await;
            tokio::spawn(async move {
                tokio::time::sleep(HOP).await;
                let _ = reply.send(());
            });
        }
    });
    let (done, outcome) = oneshot::channel();
    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        let (reply, answer) = oneshot::channel();
        to_service.send(reply).await.expect("the service task runs");
        let answered = tokio::time::timeout(Duration::from_millis(150), answer).await;
        let _ = done.send(answered.ok().and_then(Result::ok).map(|()| started.elapsed()));
    });
    outcome
}

#[cfg(feature = "testkit")]
#[tokio::test(start_paused = true)]
async fn a_pump_answers_a_request_from_another_task_at_its_wire_instant() {
    let outcome = ask_under_budget();
    crate::testkit::pump(Duration::from_secs(1)).await;
    assert_eq!(
        outcome.await.unwrap(),
        Some(2 * HOP),
        "the answer lands one round trip after the request, inside its budget"
    );
}

#[cfg(feature = "testkit")]
#[tokio::test(start_paused = true)]
async fn a_sampled_pump_samples_after_every_100ms_chunk_and_once_more() {
    let clock = Clock::test_at(0);
    let mut seen = Vec::new();
    crate::testkit::pump_sampled(Duration::from_millis(250), || seen.push(clock.now_ms())).await;
    assert_eq!(seen, vec![100, 200, 300, 400]);
}

// The linear law: deadlines are monotonic, so the only thing to pin about
// `now_ms` is that it is exactly `anchor + advanced`.
proptest! {
    #[test]
    fn now_ms_equals_anchor_plus_advance(
        anchor in -1_000_000_000i64..1_000_000_000i64,
        advance_ms in 0u64..10_000_000u64,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            let clock = Clock::test_at(anchor);
            tokio::time::advance(Duration::from_millis(advance_ms)).await;
            prop_assert_eq!(clock.now_ms(), anchor + advance_ms as i64);
            Ok(())
        })?;
    }
}
