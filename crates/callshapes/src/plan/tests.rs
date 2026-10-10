use super::*;
use crate::shapes;

fn detail(err: StepError) -> String {
    match err {
        StepError::UnexpectedKind { detail, .. } => detail,
        other => panic!("expected UnexpectedKind, got {other:?}"),
    }
}

/// Every shipped composition is structurally valid.
#[test]
fn shipped_catalog_validates() {
    let b = shapes::default_binder;
    for plan in [
        shapes::basic_call(b()),
        shapes::reinvite(b()),
        shapes::reinvite_n(b(), "reinvite10", 10),
        shapes::crossing_bye(b()),
        shapes::forked(b()),
        shapes::forked_loser_late_200(b()),
        shapes::forked_unrung(b()),
        shapes::forked_reliable(b()),
        shapes::prack_update(b()),
        shapes::rerouting_prack(b()),
        shapes::rerouting_noanswer(b()),
        shapes::options_hold(b()),
        shapes::long_call(b()),
        shapes::refer(b(), "k"),
        shapes::refer_charlie_reject(b(), "k"),
        shapes::invite_reject(b()),
        shapes::abandon_ringing(b()),
        shapes::cancel_answer_crossing(b()),
        shapes::prack_update_early(b()),
    ] {
        plan.validate().unwrap_or_else(|e| panic!("{} invalid: {e:?}", plan.id));
    }
}

/// C6: an N-cycle re-INVITE script is now valid for any `n >= 1` (the
/// per-cycle reneg counter serializes it — the `n != 1` guardrail is gone);
/// only `n == 0` (a no-op script) is rejected.
#[test]
fn reinvite_n_validates_and_rejects_zero() {
    shapes::reinvite_n(shapes::default_binder(), "reinvite10", 10)
        .validate()
        .expect("n=10 is valid now the per-cycle barrier exists");

    let mut plan = shapes::reinvite_n(shapes::default_binder(), "reinvite0", 1);
    plan.stages = vec![Stage::Script(Script::Reinvite { n: 0 })];
    assert_eq!(detail(plan.validate().unwrap_err()), "Reinvite n must be >= 1");
}

/// C5: an early UPDATE is valid only on a reliable establishment (it needs
/// the PRACKed reliable early dialog to attach to, RFC 3311 §5.1).
#[test]
fn update_early_requires_a_reliable_establishment() {
    // Reliable: valid.
    shapes::prack_update_early(shapes::default_binder())
        .validate()
        .expect("UpdateEarly on a Reliable establishment is valid");

    // Transparent (no reliable early dialog): rejected.
    let mut plan = shapes::prack_update_early(shapes::default_binder());
    plan.establish = Establishment::Transparent;
    assert_eq!(
        detail(plan.validate().unwrap_err()),
        "UpdateEarly requires a reliable establishment (early dialog)"
    );
}

/// C1/E3: Forked validation — a tagged winner and the loser must be
/// declared tags, loser distinct from winner, ≥2 tags; an unrung winner is
/// valid.
#[test]
fn forked_validates_tags() {
    let mk = |tags, winner, loser| ShapePlan {
        id: "fk",
        binder: shapes::default_binder(),
        establish: Establishment::Forked { tags, winner, reliable: false, loser_late_200: loser },
        stages: vec![],
        teardown: Teardown::CallerBye { after: DwellKnob::None, feed: ByeFeed::CheckpointAndPhase },
        ringing_gate: true,
        stamp_connected: true,
    };
    mk(&["a", "b"], ForkWinner::Tag("a"), None).validate().expect("valid fork");
    mk(&["a", "b"], ForkWinner::Unrung, None).validate().expect("an unrung winner is valid");
    mk(&["a", "b"], ForkWinner::Unrung, Some("a"))
        .validate()
        .expect("an unrung winner leaves every declared tag a loser");
    assert_eq!(
        detail(mk(&["a"], ForkWinner::Tag("a"), None).validate().unwrap_err()),
        "Forked needs >= 2 distinct fork tags"
    );
    assert_eq!(
        detail(mk(&["a", "b"], ForkWinner::Tag("c"), None).validate().unwrap_err()),
        "Forked winner must be one of the declared tags"
    );
    assert_eq!(
        detail(mk(&["a", "b"], ForkWinner::Tag("a"), Some("a")).validate().unwrap_err()),
        "Forked loser_late_200 must be a declared tag != winner"
    );
}

/// A terminal establishment admits neither stages nor a teardown, and a
/// non-terminal chain MUST tear down (the "properly terminated" rule).
#[test]
fn validate_rejects_miscomposed_chains() {
    let mut term = shapes::invite_reject(shapes::default_binder());
    term.stages = vec![Stage::Script(Script::Reinvite { n: 1 })];
    assert_eq!(
        detail(term.validate().unwrap_err()),
        "stage chained after a terminal establishment"
    );

    let mut term = shapes::abandon_ringing(shapes::default_binder());
    term.teardown =
        Teardown::CallerBye { after: DwellKnob::None, feed: ByeFeed::CheckpointAndPhase };
    assert_eq!(
        detail(term.validate().unwrap_err()),
        "teardown declared on a terminal establishment"
    );

    let mut leaky = shapes::basic_call(shapes::default_binder());
    leaky.teardown = Teardown::None;
    assert_eq!(
        detail(leaky.validate().unwrap_err()),
        "non-terminal shape with no teardown (calls must terminate)"
    );
}
