use super::*;

fn e(id: &str) -> LimiterEntry {
    LimiterEntry { id: id.into(), limit: 10 }
}

fn report(change: u64, entries: Vec<LimiterEntry>, outcome: AdmitOutcome) -> AdmitReport {
    AdmitReport { key: "c#k".into(), change, entries, outcome }
}

fn held(change: u64, entries: Vec<LimiterEntry>) -> LimiterHeld {
    LimiterHeld { change, entries }
}

fn refused(n: u64, id: &str, entries: Vec<LimiterEntry>) -> AdmitOutcome {
    AdmitOutcome::Rejected { limiter_id: id.into(), held: held(n, entries) }
}

/// The change number of a replacement that leaves.
fn sent(replacement: Replacement) -> u64 {
    match replacement {
        Replacement::Send { change, .. } => change,
        Replacement::NothingToSend { .. } => panic!("an admit leaves"),
    }
}

#[test]
fn an_admitted_set_is_held_and_targeted_and_owes_the_release() {
    let state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x"), e("y")]);
    assert!(state.counted() && !state.fail_open());
    assert_eq!(state.owed_release(), Some("c#k"));
    assert_eq!((state.held_change, state.target_change, state.change), (1, 1, 1));
}

#[test]
fn a_lost_answer_keeps_held_and_runs_open_on_what_it_asked_for() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    state.apply_admit(&report(2, vec![e("x"), e("y")], AdmitOutcome::Unavailable));
    assert_eq!(state.held(), [e("x")], "held is what the limiter last stated");
    assert!(state.counted() && state.fail_open(), "counted, y uncounted");
    let mut fresh = CallLimiterState::uncounted("c#k".into());
    fresh.apply_admit(&report(1, vec![e("x")], AdmitOutcome::NotSent));
    assert_eq!(fresh.owed_release(), None, "an unsent admit owes nothing");
    assert!(!fresh.counted() && fresh.fail_open());
    fresh.apply_admit(&report(2, vec![], AdmitOutcome::NotSent));
    assert!(!fresh.fail_open(), "a target naming no id asks for nothing");
}

#[test]
fn a_cap_refusal_leaves_the_call_on_what_the_limiter_holds() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    state.apply_admit(&report(2, vec![e("x"), e("y")], refused(2, "y", vec![e("x")])));
    assert_eq!((state.held(), state.target()), (&[e("x")][..], &[e("x")][..]));
    assert!(state.counted() && !state.fail_open(), "a refused change is not taken");
    state.apply_admit(&report(3, vec![e("y")], refused(3, "y", vec![])));
    assert!(!state.counted(), "a refusal that dropped the set");
    assert_eq!(state.owed_release(), Some("c#k"));
}

#[test]
fn an_older_statement_never_rolls_held_back() {
    let mut state = CallLimiterState::admitted("c#k".into(), 3, vec![e("z")]);
    let older = held(2, vec![e("x")]);
    assert_eq!(state.apply_refresh("c#k", Some(&older)), RefreshApplied::Stale);
    assert_eq!(state.held(), [e("z")]);
    let again = held(3, vec![e("z")]);
    assert_eq!(state.apply_refresh("c#k", Some(&again)), RefreshApplied::Unchanged);
    state.apply_admit(&report(2, vec![e("x")], AdmitOutcome::Admitted));
    assert_eq!((state.held(), state.target()), (&[e("z")][..], &[e("z")][..]));
    let newer = held(7, vec![e("y")]);
    assert_eq!(state.apply_refresh("c#k", Some(&newer)), RefreshApplied::Changed);
    assert_eq!(state.number_admit().0, 8, "the next admit is numbered above it");
}

#[test]
fn a_refresh_for_another_key_is_stale_and_one_stating_no_set_changes_nothing() {
    let mut state = CallLimiterState::admitted("c#k".into(), 3, vec![e("z")]);
    let before = state.clone();
    let newer = held(9, vec![]);
    assert_eq!(state.apply_refresh("c#earlier", Some(&newer)), RefreshApplied::Stale);
    assert_eq!(state.apply_refresh("c#k", None), RefreshApplied::Unchanged);
    assert_eq!(state, before);
}

#[test]
fn a_superseded_admit_restates_held_and_moves_the_numbers_on() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let superseded = AdmitOutcome::Superseded { held: held(4, vec![e("y")]) };
    state.apply_admit(&report(2, vec![e("z")], superseded));
    assert_eq!(state.held(), [e("y")], "what the limiter holds");
    assert_eq!(state.number_admit().0, 5);
}

/// A superseded change is not taken, like a refused one: the call runs on
/// what the limiter holds, not open on a set nobody holds.
#[test]
fn a_superseded_change_is_not_taken() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let n = sent(state.replace_set(vec![e("x"), e("z")], false));
    let superseded = AdmitOutcome::Superseded { held: held(n, vec![e("y")]) };
    state.apply_admit(&report(n, vec![e("x"), e("z")], superseded));
    assert_eq!(state.target(), [e("y")]);
    assert!(!state.fail_open(), "no set nobody holds");
}

/// A refresh stated a newer set before a refusal's report arrives: the
/// target is the held set, the newest statement, not the refusal's own.
#[test]
fn a_refusal_after_a_newer_statement_leaves_the_target_on_the_newest() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let n = sent(state.replace_set(vec![e("x"), e("y")], false));
    let newer = held(n + 3, vec![e("z")]);
    assert_eq!(state.apply_refresh("c#k", Some(&newer)), RefreshApplied::Changed);
    state.apply_admit(&report(n, vec![e("x"), e("y")], refused(n, "y", vec![e("x")])));
    assert_eq!((state.held(), state.target()), (&[e("z")][..], &[e("z")][..]));
    assert!(!state.fail_open());
}

#[test]
fn a_release_fence_empties_held() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    state.apply_admit(&report(2, vec![e("y")], AdmitOutcome::Released));
    assert!(!state.counted() && state.fail_open());
    assert_eq!(state.owed_release(), Some("c#k"));
}

#[test]
fn fail_open_counts_multiplicity() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    state.apply_admit(&report(2, vec![e("x"), e("x")], AdmitOutcome::Unavailable));
    assert!(state.fail_open(), "the second slot of x is not held");
}

#[test]
fn a_new_epoch_numbers_above_every_number_of_the_previous_one() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    state.owe_consult(4);
    state.enter_epoch();
    assert_eq!(state.number_admit().0, CHANGE_EPOCH + 1);
    state.enter_epoch();
    assert_eq!(state.number_admit().0, 2 * CHANGE_EPOCH + 1);
}

#[test]
fn a_consult_owes_the_release_and_reserves_consecutive_numbers() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    assert_eq!(state.owe_consult(6).first_change, 1);
    assert_eq!(state.owed_release(), Some("c#k"), "owed from the dispatching turn");
    assert_eq!(state.number_admit().0, 7);
}

/// A consult states whether the limiter may hold a set for the key before
/// it: never for a call that sent nothing, always once an admit request left,
/// even one whose answer was lost and left held empty.
#[test]
fn a_consult_states_whether_the_limiter_may_hold_a_set_before_it() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    assert!(!state.owe_consult(1).may_hold, "a call that sent nothing");
    assert!(state.owe_consult(1).may_hold, "the previous consult may have sent");
    let mut lost = CallLimiterState::uncounted("c#k".into());
    let (n, _) = lost.number_admit();
    lost.apply_admit(&report(n, vec![e("x")], AdmitOutcome::Unavailable));
    assert!(lost.held().is_empty());
    assert!(lost.owe_consult(1).may_hold, "the lost admit may have landed");
}

#[test]
fn an_initial_admit_s_number_owes_nothing_by_itself() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    assert_eq!(state.number_admit(), (1, LimiterHeld::default()));
    assert_eq!(state.owed_release(), None);
}

#[test]
fn a_change_is_the_target_from_its_sending_turn_and_a_refusal_takes_what_is_held() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let n = sent(state.replace_set(vec![e("x"), e("y")], false));
    assert_eq!(state.target(), [e("x"), e("y")], "the target while the change is in flight");
    state.apply_admit(&report(1, vec![e("x")], AdmitOutcome::Unavailable));
    assert_eq!(state.target(), [e("x"), e("y")], "an older report does not take it back");
    state.apply_admit(&report(n, vec![e("x"), e("y")], refused(n, "y", vec![e("x")])));
    assert_eq!(state.target(), [e("x")], "a refused change is not taken: what is held");
    assert!(!state.fail_open());
}

/// A consult's admit lands before a newer asked change is refused: the
/// refusal states what the limiter holds, the consult's set, which is the
/// target from then on; the consult's older report changes nothing.
#[test]
fn a_refused_change_takes_the_set_an_older_change_in_flight_made() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let consult = state.owe_consult(6).first_change;
    let n = sent(state.replace_set(vec![e("x"), e("y")], false));
    state.apply_admit(&report(n, vec![e("x"), e("y")], refused(n, "y", vec![e("z")])));
    state.apply_admit(&report(consult, vec![e("z")], AdmitOutcome::Admitted));
    assert_eq!((state.held(), state.target()), (&[e("z")][..], &[e("z")][..]));
    assert!(!state.fail_open(), "nothing in flight, nothing uncounted");
}

/// Two asked changes in flight both refused, the older first: the target
/// is what the limiter holds, never the older refused set.
#[test]
fn two_refused_changes_leave_the_target_on_what_is_held() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let a = sent(state.replace_set(vec![e("x"), e("y")], false));
    let b = sent(state.replace_set(vec![e("x"), e("w")], false));
    state.apply_admit(&report(a, vec![e("x"), e("y")], refused(a, "y", vec![e("x")])));
    state.apply_admit(&report(b, vec![e("x"), e("w")], refused(b, "w", vec![e("x")])));
    assert_eq!(state.target(), [e("x")]);
}

/// A replacement asking for nothing, by a call that holds and owes
/// nothing, sends no admit; once the call owes its release, or holds a
/// set, every replacement leaves and carries the held set.
#[test]
fn a_replacement_leaves_unless_nothing_can_have_landed_and_nothing_is_asked() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    assert_eq!(state.replace_set(vec![], true), Replacement::NothingToSend { change: 1 });
    assert_eq!(state.owed_release(), None);
    assert_eq!(state.runs_on(), Some(&[][..]), "moved onto nothing");
    let asked = state.replace_set(vec![e("x")], false);
    assert_eq!(asked, Replacement::Send { change: 2, held: LimiterHeld::default() });
    assert_eq!(state.owed_release(), Some("c#k"), "owed from the sending turn");
    assert_eq!(
        state.replace_set(vec![], false),
        Replacement::Send { change: 3, held: LimiterHeld::default() }
    );
    let mut counted = CallLimiterState::admitted("c#k".into(), 4, vec![e("x")]);
    assert_eq!(
        counted.replace_set(vec![], false),
        Replacement::Send { change: 5, held: held(4, vec![e("x")]) }
    );
}

/// Moved onto `[x, y]`, refused on `y`: counted on `[x]`, not fail open,
/// yet running on `y` uncounted; admitted changes that still lack `y`
/// leave it so, and the one that holds `y` ends it.
#[test]
fn a_call_moved_onto_a_refused_set_runs_uncounted_until_held_names_it() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let n = sent(state.replace_set(vec![e("x"), e("y")], true));
    assert!(state.runs_uncounted(), "in flight, y is not held");
    state.apply_admit(&report(n, vec![e("x"), e("y")], refused(n, "y", vec![e("x")])));
    assert_eq!(state.target(), [e("x")], "a refused change is not taken");
    assert!(state.counted() && !state.fail_open() && state.runs_uncounted());

    let a = state.owe_consult(1).first_change;
    state.apply_admit(&report(a, vec![e("x"), e("z")], AdmitOutcome::Admitted));
    assert!(state.runs_uncounted(), "an admitted change that does not hold y");
    let b = state.owe_consult(1).first_change;
    state.apply_admit(&report(b, vec![e("x")], AdmitOutcome::Admitted));
    assert!(state.runs_uncounted(), "an admitted return that does not hold y");
    let c = sent(state.replace_set(vec![e("z")], true));
    state.apply_admit(&report(c, vec![e("z")], AdmitOutcome::Admitted));
    assert!(!state.runs_uncounted(), "the call runs on z, which is held");
}

/// Moved onto `[x, y]` whose refusal lands after a newer change's
/// statement: the refusal is not taken, and the call still runs on `y`.
#[test]
fn a_refusal_not_taken_still_leaves_the_moved_call_uncounted() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let moved = sent(state.replace_set(vec![e("x"), e("y")], true));
    let newer = state.owe_consult(1).first_change;
    state.apply_admit(&report(newer, vec![e("x"), e("z")], AdmitOutcome::Admitted));
    state.apply_admit(&report(moved, vec![e("x"), e("y")], refused(moved, "y", vec![e("x")])));
    assert_eq!(state.held(), [e("x"), e("z")], "the refusal's statement is older");
    assert!(state.runs_uncounted());
}

/// Moved onto `[x, y]` admitted with its answer lost: uncounted until a
/// refresh states the set it holds, which ends it.
#[test]
fn a_lost_answer_of_a_moved_call_is_repaired_by_the_next_statement() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let n = sent(state.replace_set(vec![e("x"), e("y")], true));
    state.apply_admit(&report(n, vec![e("x"), e("y")], AdmitOutcome::Unavailable));
    assert!(state.runs_uncounted());
    let stated = held(n, vec![e("x"), e("y")]);
    assert_eq!(state.apply_refresh("c#k", Some(&stated)), RefreshApplied::Changed);
    assert!(!state.runs_uncounted());
}

/// A fold that runs its route moves the call onto the route's set
/// whatever the outcome; a resolution after a refused route runs the call
/// on its target again.
#[test]
fn a_resolution_after_a_refused_route_runs_the_call_on_its_target_again() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let failover = state.owe_consult(1).first_change;
    let route = vec![e("x"), e("y")];
    state.apply_fold(&report(failover, route.clone(), AdmitOutcome::Unavailable), true);
    assert_eq!(state.runs_on(), Some(&route[..]), "the call runs on its route");
    assert!(state.runs_uncounted());
    let release = state.owe_consult(1).first_change;
    state.apply_fold(&report(release, route, refused(release, "y", vec![e("x")])), false);
    assert_eq!(state.runs_on(), None);
    assert!(!state.runs_uncounted(), "the call runs on its target, which is held");
}

#[test]
fn runs_on_counts_multiplicity() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let n = state.owe_consult(1).first_change;
    let superseded = AdmitOutcome::Superseded { held: held(n, vec![e("x")]) };
    state.apply_fold(&report(n, vec![e("x"), e("x")], superseded), true);
    assert!(!state.fail_open() && state.runs_uncounted(), "the second slot of x is not held");
}

#[test]
fn a_lost_admit_reports_the_target_it_asked_for_while_no_later_change_replaced_it() {
    let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
    let n = sent(state.replace_set(vec![e("x"), e("y")], false));
    assert_eq!(state.lost_admit(n), AdmitReport::lost("c#k".into(), n, vec![e("x"), e("y")]));
    sent(state.replace_set(vec![e("z")], false));
    assert_eq!(state.lost_admit(n), AdmitReport::lost("c#k".into(), n, vec![]));
}

/// A call whose consults never sent an admit — each route stated no limiter
/// and nothing could have landed for the key — sends none on any number of
/// later empty routes, consult after consult.
#[test]
fn consults_that_sent_nothing_leave_nothing_the_limiter_may_hold() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    for _ in 0..3 {
        let consult = state.owe_consult(1);
        assert!(!consult.may_hold, "nothing can have landed for the key");
        state.apply_fold(&report(consult.first_change, vec![], AdmitOutcome::NotSent), true);
    }
    assert!(!state.may_hold());
    assert_eq!(state.owed_release(), Some("c#k"), "the end-of-call release stays owed");
}

/// A fold that arrives once no consult awaits it still closes its consult: a
/// later empty route sends no replacement.
#[test]
fn an_unawaited_fold_closes_its_consult() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    let consult = state.owe_consult(1);
    state.apply_unawaited_fold(&report(consult.first_change, vec![], AdmitOutcome::NotSent));
    assert!(!state.may_hold());
    assert!(!state.owe_consult(1).may_hold, "nothing can have landed for the key");
}

/// A consult whose fold the record never applied — lost with the node that
/// dispatched it — may have sent its admit: the limiter may hold a set.
#[test]
fn a_consult_whose_fold_is_missing_may_hold_a_set() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    state.owe_consult(1);
    assert!(state.may_hold());
    assert!(state.owe_consult(1).may_hold, "a second consult sends its empty replacement");
}

/// A consult that sent an admit leaves the key one the limiter may hold, its
/// answer lost or not.
#[test]
fn a_consult_that_sent_an_admit_leaves_a_set_the_limiter_may_hold() {
    let mut state = CallLimiterState::uncounted("c#k".into());
    let consult = state.owe_consult(1);
    state.apply_fold(&report(consult.first_change, vec![e("x")], AdmitOutcome::Unavailable), true);
    assert!(state.may_hold());
}
