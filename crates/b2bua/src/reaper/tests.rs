use super::*;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support;
use crate::store::InMemoryCallStore;
use sip_ids::TurnIds;

/// The outcomes of the verdicts sent so far.
fn outcomes(rx: &mut mpsc::UnboundedReceiver<CallEvent>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(CallEvent::InternalEvent { outcome, .. }) = rx.try_recv() {
        out.push(outcome);
    }
    out
}

/// A call flooded past its dispatch overflow is condemned once however
/// many refusals report it, then swept every pass on its own ladder — the
/// in-flight body aborted, then the discharge — while it stays resident
/// and Active.
#[tokio::test]
async fn an_overflowing_call_is_condemned_once_and_swept_until_gone() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let metrics = B2buaMetrics::new();
    let reaper = Reaper::new(true, 1, 3_600_000, tx, metrics.clone());
    let state = CallState::new(Arc::new(InMemoryCallStore::new()), "w0", metrics.clone());
    let src = "10.0.0.9:5060".parse().unwrap();
    let invite = test_support::invite("w0", "w1", "flood");
    let call_ref = state.create(build_initial_call(
        &invite,
        src,
        &B2buaConfig::default(),
        &mut sip_ids::TurnIds::fixed(1),
        0,
    ));
    let in_flight = InFlight::default();

    reaper.on_overflow_ceiling(&call_ref);
    reaper.on_overflow_ceiling(&call_ref);
    assert_eq!(outcomes(&mut rx), vec![OUTCOME_OVERFLOW], "one verdict per condemned call");

    for _ in 0..5 {
        reaper.maybe_sweep(&state, &in_flight, 0);
    }
    assert_eq!(
        outcomes(&mut rx),
        vec![
            OUTCOME_OVERFLOW,
            OUTCOME_OVERFLOW,
            OUTCOME_OVERFLOW,
            OUTCOME_OVERFLOW,
            OUTCOME_DISCHARGE
        ]
    );

    state.remove(&call_ref);
    reaper.maybe_sweep(&state, &in_flight, 0);
    assert!(outcomes(&mut rx).is_empty(), "a gone call is no longer swept");
    reaper.on_overflow_ceiling(&call_ref);
    assert_eq!(outcomes(&mut rx), vec![OUTCOME_OVERFLOW], "its condemnation was pruned");
}

#[test]
fn verdict_confirm_matrix() {
    // Stale verdict: applies only on an exact stamp match.
    assert!(verdict_confirmed(OUTCOME_STALE, Some(5), Some(5)));
    assert!(!verdict_confirmed(OUTCOME_STALE, Some(5), Some(9)), "touched since the sweep");
    assert!(!verdict_confirmed(OUTCOME_STALE, Some(5), None), "released — never resurrect");
    assert!(!verdict_confirmed(OUTCOME_STALE, None, Some(5)), "malformed verdict");
    // Failure-hook verdicts: apply iff the call is still resident.
    assert!(verdict_confirmed(OUTCOME_FATAL, None, Some(5)));
    assert!(!verdict_confirmed(OUTCOME_FATAL, None, None));
    assert!(verdict_confirmed(OUTCOME_DISCHARGE, None, Some(5)));
    assert!(!verdict_confirmed(OUTCOME_DISCHARGE, None, None));
}

fn condemned_rig(
    calls: usize,
) -> (Reaper, mpsc::UnboundedReceiver<CallEvent>, CallState, Vec<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let metrics = B2buaMetrics::new();
    let reaper = Reaper::new(true, 1, 60_000, tx, metrics.clone());
    let state = CallState::new(Arc::new(InMemoryCallStore::new()), "w0", metrics);
    let src = "10.0.0.9:5060".parse().unwrap();
    let refs = (0..calls)
        .map(|n| {
            let invite = test_support::invite("w0", "w1", &format!("flood{n}"));
            state.create(build_initial_call(
                &invite,
                src,
                &B2buaConfig::default(),
                &mut TurnIds::fixed(1),
                0,
            ))
        })
        .collect();
    (reaper, rx, state, refs)
}

/// A sweep sends at most `MAX_VERDICTS_PER_SWEEP` verdicts, condemned
/// calls included.
#[tokio::test]
async fn the_condemned_loop_shares_the_sweep_budget() {
    let (reaper, mut rx, state, refs) = condemned_rig(MAX_VERDICTS_PER_SWEEP + 6);
    for r in &refs {
        reaper.on_overflow_ceiling(r);
    }
    let _ = outcomes(&mut rx);
    let in_flight = InFlight::default();
    reaper.maybe_sweep(&state, &in_flight, 0);
    assert_eq!(outcomes(&mut rx).len(), MAX_VERDICTS_PER_SWEEP);
}

/// A call both stale and condemned climbs each ladder one rung per sweep:
/// three sweeps reach neither discharge.
#[tokio::test]
async fn stale_and_overflow_verdicts_keep_their_own_ladders() {
    let (reaper, mut rx, state, refs) = condemned_rig(1);
    reaper.on_overflow_ceiling(&refs[0]);
    let _ = outcomes(&mut rx);
    let in_flight = InFlight::default();
    let far = i64::MAX / 2;
    for _ in 0..3 {
        reaper.maybe_sweep(&state, &in_flight, far);
    }
    let sent = outcomes(&mut rx);
    assert_eq!(sent.len(), 6, "{sent:?}");
    assert!(!sent.iter().any(|o| o == OUTCOME_DISCHARGE), "{sent:?}");
}

/// A condemned call whose teardown has begun gets no further verdict.
#[tokio::test]
async fn a_condemned_call_that_left_active_is_not_swept() {
    let (reaper, mut rx, state, refs) = condemned_rig(1);
    reaper.on_overflow_ceiling(&refs[0]);
    let _ = outcomes(&mut rx);
    let mut call = state.peek(&refs[0]).unwrap();
    call.state = CallModelState::Terminating;
    state.update(call);
    let in_flight = InFlight::default();
    reaper.maybe_sweep(&state, &in_flight, 0);
    assert!(outcomes(&mut rx).is_empty());
}

/// The overflow ladder's own abort of a hung body is no handler failure:
/// it strikes nothing, so no `fatal-error` verdict forces the legs the
/// queued overflow teardown is about to end on the wire.
#[tokio::test]
async fn the_overflow_ladders_abort_strikes_nothing() {
    let (reaper, mut rx, state, refs) = condemned_rig(1);
    let call_ref = refs[0].clone();
    let dispatcher = crate::dispatch::PerCallDispatcher::new(1, 1, 8, B2buaMetrics::new())
        .with_failure_hook(reaper.failure_hook());
    let hung: crate::dispatch::DispatchBody = Box::pin(std::future::pending());
    let _ = dispatcher.offer(&call_ref, hung, crate::dispatch::DispatchClass::Internal);
    let in_flight = dispatcher.in_flight();
    tokio::task::yield_now().await;
    reaper.on_overflow_ceiling(&call_ref);
    for _ in 0..ESCALATE_ABORT_AFTER + 1 {
        reaper.maybe_sweep(&state, &in_flight, 0);
    }
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let sent = outcomes(&mut rx);
    assert!(sent.len() > ESCALATE_ABORT_AFTER as usize, "the ladder reached its abort: {sent:?}");
    assert!(!sent.iter().any(|o| o == OUTCOME_FATAL), "{sent:?}");
}

/// A takeover copy is its self-release's (ADR-0020 X3): condemned or not,
/// the sweep sends it nothing.
#[tokio::test]
async fn a_condemned_takeover_copy_is_not_swept() {
    let (reaper, mut rx, state, refs) = condemned_rig(1);
    reaper.on_overflow_ceiling(&refs[0]);
    let _ = outcomes(&mut rx);
    state.mark_takeover(&refs[0]);
    let in_flight = InFlight::default();
    reaper.maybe_sweep(&state, &in_flight, 0);
    assert!(outcomes(&mut rx).is_empty());
}

/// A call inserted without its stamp (an insert a panic cut short) is found
/// by the next sweep, stamped at the sweep's instant and counted; the pass
/// after it finds the store whole.
#[tokio::test]
async fn a_call_inserted_without_its_stamp_is_stamped_by_the_next_sweep() {
    let (reaper, _rx, state, refs) = condemned_rig(2);
    state.cut_stamp(&refs[0]);
    let in_flight = InFlight::default();
    reaper.maybe_sweep(&state, &in_flight, 77);
    assert_eq!(state.last_touched(&refs[0]), Some(77));
    assert_eq!(reaper.metrics.store_reconciled_missing_stamps_total(), 1);
    reaper.maybe_sweep(&state, &in_flight, 78);
    assert_eq!(reaper.metrics.store_reconciled_missing_stamps_total(), 1, "counted once");
}

/// The stamp and the index entries of a call gone from the call map (a
/// removal a panic cut short) are dropped by the next sweep and counted.
#[tokio::test]
async fn the_orphan_entries_of_a_gone_call_are_dropped_by_the_next_sweep() {
    let (reaper, _rx, state, refs) = condemned_rig(2);
    state.cut_call(&refs[0]);
    assert!(state.indexes(&refs[0]));
    let in_flight = InFlight::default();
    reaper.maybe_sweep(&state, &in_flight, 5);
    assert!(!state.indexes(&refs[0]), "no index entry names the gone call");
    assert_eq!(state.last_touched(&refs[0]), None);
    assert_eq!(state.touched_count(), 1);
    assert_eq!(reaper.metrics.store_reconciled_orphan_stamps_total(), 1);
    assert!(reaper.metrics.store_reconciled_orphan_index_total() >= 1);
    assert!(state.indexes(&refs[1]), "the live call keeps its keys");
}

/// A wave of stale calls filling the sweep budget does not starve a
/// condemned call.
#[tokio::test]
async fn stale_calls_leave_condemned_calls_their_share_of_the_sweep() {
    let (reaper, mut rx, state, refs) = condemned_rig(MAX_VERDICTS_PER_SWEEP + 6);
    reaper.on_overflow_ceiling(&refs[0]);
    let _ = outcomes(&mut rx);
    let in_flight = InFlight::default();
    reaper.maybe_sweep(&state, &in_flight, i64::MAX / 2);
    let sent = outcomes(&mut rx);
    assert_eq!(sent.len(), MAX_VERDICTS_PER_SWEEP);
    assert!(sent.iter().any(|o| o == OUTCOME_OVERFLOW), "the condemned call is swept");
}
