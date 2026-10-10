use super::*;
use call::TimerType;

#[tokio::test(start_paused = true)]
async fn fires_after_delay_under_paused_clock() {
    let clock = Clock::test_at(0);
    let (timers, mut fire_rx) = TimerService::spawn(clock);
    timers
        .schedule(
            TimerEntry {
                key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
                generation: 4,
                fire_at: 5_000,
            },
            "w0|cid|tag".into(),
            Some("w0|cid|tag#k".into()),
        )
        .await;
    // Nothing yet.
    tokio::time::advance(Duration::from_millis(4_000)).await;
    assert!(fire_rx.try_recv().is_err());
    // Cross the deadline.
    tokio::time::advance(Duration::from_millis(1_500)).await;
    let ev = fire_rx.recv().await.unwrap();
    match ev {
        CallEvent::Timer { key, generation, call_ref, incarnation } => {
            assert_eq!(key, TimerKey::leg(TimerType::NoAnswer, "b-1"));
            assert_eq!(generation, 4, "the fire names the instance it was armed under");
            assert_eq!(call_ref, "w0|cid|tag");
            assert_eq!(incarnation.as_deref(), Some("w0|cid|tag#k"), "the scheduler's");
        }
        _ => panic!("expected timer event"),
    }
}

#[tokio::test(start_paused = true)]
async fn cancel_prevents_fire() {
    let clock = Clock::test_at(0);
    let (timers, mut fire_rx) = TimerService::spawn(clock);
    timers
        .schedule(
            TimerEntry { key: TimerKey::call(TimerType::Keepalive), generation: 1, fire_at: 1_000 },
            "c".into(),
            None,
        )
        .await;
    timers.cancel("c".into(), TimerKey::call(TimerType::Keepalive)).await;
    tokio::time::advance(Duration::from_millis(2_000)).await;
    // Give the driver a tick to process.
    tokio::task::yield_now().await;
    assert!(fire_rx.try_recv().is_err());
}

/// `DelayQueue` `Key` aliasing: a timer fires (freeing its slab slot), a
/// second timer reuses that slot, the first is rescheduled, then the second
/// is cancelled. A cancel holding a stale `Key` would alias the freed slot
/// and evict the *rescheduled* first timer, which would then silently never
/// fire. The rescheduled timer must fire.
#[tokio::test(start_paused = true)]
async fn reschedule_survives_aliasing_cancel() {
    let clock = Clock::test_at(0);
    let (timers, mut fire_rx) = TimerService::spawn(clock);
    let cref = "w0|cid|tag".to_string();

    // 1. Arm "keepalive" for t=30s and let it fire (slot freed).
    timers
        .schedule(
            TimerEntry {
                key: TimerKey::call(TimerType::Keepalive),
                generation: 1,
                fire_at: 30_000,
            },
            cref.clone(),
            None,
        )
        .await;
    tokio::time::advance(Duration::from_millis(30_000)).await;
    assert!(
        matches!(
            fire_rx.recv().await,
            Some(CallEvent::Timer { ref key, .. }) if key.timer_type == TimerType::Keepalive
        ),
        "first keepalive fires at 30s",
    );

    // 2. Arm a per-leg timeout (reuses the freed slot) ...
    timers
        .schedule(
            TimerEntry {
                key: TimerKey::leg(TimerType::KeepaliveTimeout, "a"),
                generation: 1,
                fire_at: 35_000,
            },
            cref.clone(),
            None,
        )
        .await;
    // 3. ... reschedule keepalive for t=60s ...
    timers
        .schedule(
            TimerEntry {
                key: TimerKey::call(TimerType::Keepalive),
                generation: 1,
                fire_at: 60_000,
            },
            cref.clone(),
            None,
        )
        .await;
    // 4. ... and cancel the timeout (the aliasing trigger).
    timers.cancel(cref.clone(), TimerKey::leg(TimerType::KeepaliveTimeout, "a")).await;

    // The cancelled timeout must NOT fire; the rescheduled keepalive MUST.
    tokio::time::advance(Duration::from_millis(30_000)).await; // → t=60s
    let ev = fire_rx.recv().await;
    assert!(
        ev.as_ref().is_some_and(|e| matches!(
            e,
            CallEvent::Timer { key, .. } if key.timer_type == TimerType::Keepalive
        )),
        "rescheduled keepalive survives the aliasing cancel and fires at 60s, got {ev:?}",
    );
    // Nothing else pending (the timeout was a dropped tombstone).
    assert!(fire_rx.try_recv().is_err(), "cancelled timeout must not fire");
}

/// Let the single-task driver drain pending commands and run its end-of-loop
/// gauge update. `schedule`/`cancel` only enqueue; yielding hands the runtime
/// to the driver task (current-thread under `start_paused`).
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// Cancelling a timer must physically reclaim its `DelayQueue` slot *now*,
/// not leave it to expire at its original deadline. Models a long
/// `GlobalDuration` timer (1 h) armed on a call that tears down seconds
/// later: a logical-only cancel would leave the slot ~1 h, so at load the
/// queue would grow to hundreds of thousands of dead entries. `queue_len`
/// must drop to 0 on the teardown cancel, well before the 1 h deadline.
#[tokio::test(start_paused = true)]
async fn cancel_physically_reclaims_the_queue_slot() {
    let clock = Clock::test_at(0);
    let metrics = B2buaMetrics::new();
    let (timers, _fire_rx) = TimerService::spawn_with_metrics(clock, metrics.clone());

    timers
        .schedule(
            TimerEntry {
                key: TimerKey::call(TimerType::GlobalDuration),
                generation: 1,
                fire_at: 3_600_000,
            },
            "c".into(),
            None,
        )
        .await;
    settle().await;
    assert_eq!(metrics.timer_queue_len(), 1, "armed timer occupies a queue slot");
    assert_eq!(metrics.timer_live(), 1);

    // Call teardown — the slot must be freed immediately, not at +1 h.
    timers.cancel_all("c".into()).await;
    settle().await;
    assert_eq!(
        metrics.timer_queue_len(),
        0,
        "CancelAll reclaims the slot — no lingering tombstone"
    );
    assert_eq!(metrics.timer_live(), 0, "no schedulable timers remain");
}

/// A reschedule must also reclaim the superseded slot, so a periodically
/// re-armed timer (the keepalive) keeps `queue_len` flat instead of leaking
/// one tombstone per re-arm.
#[tokio::test(start_paused = true)]
async fn reschedule_does_not_accumulate_tombstones() {
    let clock = Clock::test_at(0);
    let metrics = B2buaMetrics::new();
    let (timers, _fire_rx) = TimerService::spawn_with_metrics(clock, metrics.clone());

    for round in 0..50 {
        timers
            .schedule(
                TimerEntry {
                    key: TimerKey::call(TimerType::Keepalive),
                    generation: 1,
                    fire_at: 300_000 + round,
                },
                "c".into(),
                None,
            )
            .await;
    }
    settle().await;
    assert_eq!(
        metrics.timer_queue_len(),
        1,
        "50 re-arms collapse to one live entry, not 50 tombstones"
    );
    assert_eq!(metrics.timer_live(), 1);
}

/// A burst of more timers than a bounded fire channel would hold (1024) must
/// deliver EVERY fire — the `DelayQueue` is the only bound, not an in-front
/// buffer. A `try_send` into `channel(1024)` would silently drop the overflow
/// (a lost keepalive/no-answer/max-duration).
#[tokio::test(start_paused = true)]
async fn timer_flood_past_old_channel_cap_delivers_every_fire() {
    let clock = Clock::test_at(0);
    let (timers, mut fire_rx) = TimerService::spawn(clock);

    const N: usize = 3_000; // comfortably past a 1024 cap
    for i in 0..N {
        timers
            .schedule(
                TimerEntry {
                    key: TimerKey::call(TimerType::Keepalive),
                    generation: 1,
                    fire_at: 1_000,
                },
                format!("call-{i}"),
                None,
            )
            .await;
    }

    // Cross the single deadline so all N fire in one go, with NO interleaved
    // draining (the worst case for a bounded channel).
    tokio::time::advance(Duration::from_millis(1_500)).await;

    // Drain, letting the driver flush the DelayQueue between empties. Stop
    // when all N are in or the channel stays empty across many settle passes.
    let mut got = 0;
    let mut idle = 0;
    while got < N && idle < 64 {
        match fire_rx.try_recv() {
            Ok(_) => {
                got += 1;
                idle = 0;
            }
            Err(_) => {
                idle += 1;
                tokio::task::yield_now().await;
            }
        }
    }
    assert_eq!(got, N, "every fire delivered — no silent overflow drop");
}

/// Cross-call timer-id aliasing. The timer service is a single shared
/// driver, but timer ids are per-call and repeat across calls (every
/// established call arms a `"Keepalive"` timer). An id-only `active` map
/// would let a second call's `"Keepalive"` (same id, different call_ref)
/// overwrite the first's live epoch, so the first call's keepalive would
/// silently never fire — at scale keepalives stop, dead peers are never
/// probed, and `active_calls` grows without bound. Both calls'
/// identically-named timers must fire independently.
#[tokio::test(start_paused = true)]
async fn colliding_timer_ids_across_calls_both_fire() {
    let clock = Clock::test_at(0);
    let (timers, mut fire_rx) = TimerService::spawn(clock);

    // Two distinct calls, IDENTICAL timer key — the production shape.
    let call_a = "w0|call-a|tag-a".to_string();
    let call_b = "w0|call-b|tag-b".to_string();
    for cref in [&call_a, &call_b] {
        timers
            .schedule(
                TimerEntry {
                    key: TimerKey::call(TimerType::Keepalive),
                    generation: 1,
                    fire_at: 30_000,
                },
                cref.clone(),
                None,
            )
            .await;
    }

    tokio::time::advance(Duration::from_millis(30_000)).await;

    let mut fired: Vec<String> = Vec::new();
    for _ in 0..2 {
        match fire_rx.recv().await {
            Some(CallEvent::Timer { key, call_ref, .. })
                if key.timer_type == TimerType::Keepalive =>
            {
                fired.push(call_ref)
            }
            other => panic!("expected a keepalive fire, got {other:?}"),
        }
    }
    fired.sort();
    assert_eq!(
        fired,
        vec![call_a, call_b],
        "both calls' keepalives fire — no cross-call aliasing tombstone",
    );
}

/// Cancelling one call's timer must not cancel another call's identically
/// named timer: an id-only cancel would let `cancel("Keepalive")` for call A
/// wipe call B's `"Keepalive"` epoch too.
#[tokio::test(start_paused = true)]
async fn cancel_is_scoped_to_its_call() {
    let clock = Clock::test_at(0);
    let (timers, mut fire_rx) = TimerService::spawn(clock);
    let call_a = "w0|call-a|tag-a".to_string();
    let call_b = "w0|call-b|tag-b".to_string();
    for cref in [&call_a, &call_b] {
        timers
            .schedule(
                TimerEntry {
                    key: TimerKey::call(TimerType::Keepalive),
                    generation: 1,
                    fire_at: 30_000,
                },
                cref.clone(),
                None,
            )
            .await;
    }
    // Cancel only call A's keepalive.
    timers.cancel(call_a.clone(), TimerKey::call(TimerType::Keepalive)).await;

    tokio::time::advance(Duration::from_millis(30_000)).await;

    // Exactly call B's keepalive must fire.
    match fire_rx.recv().await {
        Some(CallEvent::Timer { key, call_ref, .. }) if key.timer_type == TimerType::Keepalive => {
            assert_eq!(call_ref, call_b, "call B's keepalive survives call A's cancel");
        }
        other => panic!("expected call B's keepalive, got {other:?}"),
    }
    tokio::task::yield_now().await;
    assert!(fire_rx.try_recv().is_err(), "call A's keepalive stays cancelled");
}
