// ── ADR-0020 X7: obligation-set equivalence gate ────────────────────────────
//
// Over arbitrary call snapshots (states × limiter state incl. fail-open ×
// pre-emitted effects), `invariants::enforce(&ObligationSet::core(), …)` produces
// the same call and effect lanes as `reference_enforce`, which states the
// limiter and CDR obligations inline (its limiter dedupe counted per hold).
use super::fixtures::*;
use super::*;
use b2bua_core::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, HandlerEffects, SoftBoundedEffect,
};
use b2bua_core::obligations::ObligationSet;
use call::{CallLimiterState, TimerEntry};
use proptest::prelude::*;

/// The limiter and CDR obligations written inline (ADR-0010 X5 shape).
fn reference_enforce(before: &call::Call, mut result: HandlerResult) -> HandlerResult {
    let became_terminated = before.state != CallModelState::Terminated
        && result.call.state == CallModelState::Terminated;
    if !became_terminated {
        return result;
    }
    let crit = &mut result.effects.critical;
    if !crit.iter().any(|e| matches!(e, CriticalStateEffect::CancelAllTimers)) {
        crit.insert(0, CriticalStateEffect::CancelAllTimers);
    }
    result.call.timers.clear();

    if !result.effects.buffered.iter().any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr)) {
        result.effects.buffered.push(BufferedObservabilityEffect::WriteCdr);
    }

    // One release per call that sent an admit: a pre-emitted release
    // discharges it.
    let already =
        result.effects.soft.iter().any(
            |e| matches!(e, SoftBoundedEffect::ReleaseLimiter { key } if *key == result.call.limiter.key()),
        );
    if result.call.limiter.owed_release().is_some() && !already {
        result
            .effects
            .soft
            .push(SoftBoundedEffect::ReleaseLimiter { key: result.call.limiter.key().to_string() });
    }

    result.effects.critical.retain(|e| !matches!(e, CriticalStateEffect::RemoveCall));
    result.effects.critical.push(CriticalStateEffect::RemoveCall);
    result
}

fn arb_state() -> impl Strategy<Value = CallModelState> {
    prop_oneof![
        Just(CallModelState::Active),
        Just(CallModelState::Terminating),
        Just(CallModelState::Terminated),
    ]
}

fn arb_limiter_state() -> impl Strategy<Value = CallLimiterState> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        proptest::collection::vec(prop_oneof![Just("l1"), Just("l2")], 0..3),
    )
        .prop_map(|(counted, sent, fail_open, ids)| {
            let entries: Vec<call::LimiterEntry> = ids
                .into_iter()
                .map(|id| call::LimiterEntry { id: id.to_string(), limit: 10 })
                .collect();
            let held = if counted { entries.clone() } else { Vec::new() };
            let target = if fail_open { entries } else { held.clone() };
            CallLimiterState::from_parts(
                "call#k".into(),
                counted || sent,
                (held, 1),
                (target, 1),
                1,
                None,
            )
        })
}

/// A pre-emitted rule release (the dedupe case).
fn arb_pre_release() -> impl Strategy<Value = SoftBoundedEffect> {
    Just(SoftBoundedEffect::ReleaseLimiter { key: "call#k".into() })
}

proptest! {
    #[test]
    fn extracted_enforce_is_equivalent_to_the_old_body(
        before_state in arb_state(),
        after_state in arb_state(),
        limiter in arb_limiter_state(),
        pre_releases in proptest::collection::vec(arb_pre_release(), 0..3),
        pre_write_cdr in proptest::bool::ANY,
        pre_cancel_all in proptest::bool::ANY,
        pre_remove_call in proptest::bool::ANY,
        timer_count in 0..3usize,
    ) {
        let mut before = test_call();
        before.state = before_state;

        let mut after = test_call();
        after.state = after_state;
        if after_state != CallModelState::Active {
            // A synthetic terminal states its cause as every live path does.
            call::helpers::record_termination(&mut after,
                0,
                TerminationCause::Supervisor,
                None,
            );
        }
        after.limiter = limiter;
        after.timers = (0..timer_count)
            .map(|i| TimerEntry {
                key: TimerKey::call(TimerType::Keepalive),
                generation: 1,
                fire_at: 1_000 + i as i64,
            })
            .collect();

        let mut effects = HandlerEffects::new();
        if pre_cancel_all {
            effects.critical.push(CriticalStateEffect::CancelAllTimers);
        }
        if pre_remove_call {
            effects.critical.push(CriticalStateEffect::RemoveCall);
        }
        effects.critical.push(CriticalStateEffect::CancelTimer {
            key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
        });
        effects.soft.extend(pre_releases);
        if pre_write_cdr {
            effects.buffered.push(BufferedObservabilityEffect::WriteCdr);
        }

        let result = HandlerResult { call: after, effects };

        let reference = reference_enforce(&before, result.clone());
        // `answer_unanswered_a_leg = false`: the reference states the limiter
        // and CDR obligations only; the ADR-0022 unanswered-a-leg final is
        // additive and covered by its own tests above.
        let actual = invariants::enforce(
            &ObligationSet::core(),
            &before,
            result,
            0,
            invariants::UnansweredCaller::Leave,
            &mut TurnNotes::default(),
        );

        prop_assert_eq!(
            &reference.call,
            &actual.call,
            "call (incl. cleared timers) must match"
        );
        let lanes = |r: &HandlerResult| {
            format!("{:?}", (&r.effects.critical, &r.effects.soft, &r.effects.buffered))
        };
        prop_assert_eq!(
            lanes(&reference),
            lanes(&actual),
            "effect lanes must be identical (order included)"
        );
    }
}
