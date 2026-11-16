//! The held-callee mutation: a plain answering callee becomes a held one with
//! drawn forks, reliability and timing, and the caller may CANCEL at a drawn
//! moment.

use std::time::Duration;

use scenario_harness::actor::{
    ActorCall, Barrier, Disposition, ForkWinner, Goal, GoalStep, LegPhase, StateInner,
};

use crate::Rng;

const FORKS2: &[&str] = &["fk1", "fk2"];
const FORKS3: &[&str] = &["fk1", "fk2", "fk3"];

/// Whether `d` answers on its own, and reliably (RFC 3262).
fn plain_answer(d: &Disposition) -> Option<bool> {
    match d {
        Disposition::Answer | Disposition::RingThenAnswer { .. } => Some(false),
        Disposition::ReliableAnswer => Some(true),
        _ => None,
    }
}

/// Hold the plain answering callees among `roles` that have no goals of their
/// own, each with 60 % chance: it rings (unforked, two or three forks) within
/// 400 ms, reliably when it answered reliably or, `reliable_ok` (the leg is
/// offered 100rel), with 40 % chance, then answers within 1.2 s on a drawn
/// fork or on one that never rang. Returns whether any held callee rings
/// reliably.
pub fn hold_callees(
    rng: &mut Rng,
    call: &mut ActorCall,
    roles: &[&str],
    reliable_ok: bool,
    notes: &mut Vec<String>,
) -> bool {
    let mut any_reliable = false;
    for actor in &mut call.actors {
        let Some(was_reliable) = plain_answer(&actor.disposition) else { continue };
        if !roles.contains(&actor.role) || !actor.goals.is_empty() || rng.chance(40) {
            continue;
        }
        let tags: &'static [&'static str] = match rng.below(3) {
            0 => &[],
            1 => FORKS2,
            _ => FORKS3,
        };
        let reliable = was_reliable || (reliable_ok && rng.chance(40));
        any_reliable |= reliable;
        let winner = match tags {
            [] => None,
            _ if rng.chance(25) => Some(ForkWinner::Unrung),
            _ => Some(ForkWinner::Tag(rng.pick(tags))),
        };
        actor.disposition = Disposition::Held;
        actor.goals = vec![
            Goal::new(Barrier::None, GoalStep::Ring { forks: tags, reliable })
                .after(Duration::from_millis(rng.below(400))),
            Goal::new(Barrier::None, GoalStep::Answer { winner })
                .after(Duration::from_millis(rng.below(1200))),
        ];
        notes.push(format!(
            "held({}):forks={} reliable={reliable} winner={winner:?}",
            actor.role,
            tags.len()
        ));
    }
    any_reliable
}

/// With 25 % chance, the caller CANCELs within 3 s of its first provisional,
/// unless the call is confirmed by then (RFC 3261 §9.1). Returns whether the
/// CANCEL was drawn: the call's accepted endings are then the CANCEL races.
pub fn inject_cancel(rng: &mut Rng, call: &mut ActorCall, notes: &mut Vec<String>) -> bool {
    if !rng.chance(25) {
        return false;
    }
    let caller = scenario_harness::actor::originating_role(&call.actors);
    let Some(actor) = call.actors.iter_mut().find(|a| a.role == caller) else { return false };
    let at = Duration::from_millis(rng.below(3000));
    let provisional = move |s: &StateInner| s.leg(caller).heard_provisional();
    let confirmed = move |s: &StateInner| s.leg_at_least(caller, LegPhase::Confirmed);
    let cancel = Goal::new(
        Barrier::pred("fz_provisional", provisional),
        GoalStep::Cancel { stated: vec![] },
    )
    .after(at)
    .unless(Barrier::pred("fz_confirmed", confirmed));
    actor.goals.insert(1.min(actor.goals.len()), cancel);
    notes.push(format!("cancel+{}ms", at.as_millis()));
    true
}

/// Every INVITE of `call` offers `100rel` (RFC 3262 §3), so a reliable
/// provisional is legal on every leg it reaches.
pub fn offer_100rel(call: &mut ActorCall) {
    for g in call.actors.iter_mut().flat_map(|a| a.goals.iter_mut()) {
        if let GoalStep::Invite { plan: Some(plan), .. } = &mut g.step {
            let offered = plan
                .headers
                .iter()
                .any(|(h, v)| h.eq_ignore_ascii_case("Supported") && v.contains("100rel"));
            if !offered {
                *plan = plan.clone().with_supported_100rel();
            }
        }
    }
}
