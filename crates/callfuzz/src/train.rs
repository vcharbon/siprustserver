//! Provisional trains on held callees (RFC 3262 §3).
//!
//! A held callee's single `Ring` becomes a drawn train: per early dialog, a
//! run of 180s and 183s, with or without SDP, reliable or not, in one global
//! order interleaved across forks with drawn gaps. Each reliable one's PRACK
//! is answered after a drawn hold, so PRACKs of several forks are in flight
//! across one another; the final may race the PRACKs RFC 3262 §3 lets it.

use std::time::Duration;

use scenario_harness::actor::{
    ActorCall, Barrier, Disposition, Goal, GoalStep, HeldFinal, Provisional,
};

use crate::held::offer_100rel;
use crate::Rng;

/// Provisionals on one early dialog, at most.
pub const PER_DIALOG: usize = 15;

/// The salt of the train stream: a call's trains draw from their own
/// generator, so the rest of its draw is the same with trains on or off.
const TRAIN_SALT: u64 = 0x7EA1_5EED_7EA1_5EED;

/// One drawn element of a train.
#[derive(Debug, Clone)]
pub struct Elem {
    /// The index of the early dialog (fork) it is sent on.
    pub fork: usize,
    pub p: Provisional,
    /// The wait after the element before it.
    pub gap: Duration,
}

/// The train of one held callee over `forks` (one early dialog each), in
/// global order: every dialog has 1 to [`PER_DIALOG`] elements, `max` in all
/// at most (one per dialog at least); a dialog's first reliable element carries
/// SDP (RFC 3261 §13.2.1, the answer); `each_reliable` gives every dialog a
/// reliable element. No element is reliable unless `allow_reliable` or
/// `each_reliable`.
pub fn draw(
    rng: &mut Rng,
    forks: &[Option<&'static str>],
    max: usize,
    allow_reliable: bool,
    each_reliable: bool,
) -> Vec<Elem> {
    let n = forks.len();
    let cap = (n * PER_DIALOG).min(max.max(n));
    let total = match rng.below(10) {
        0..=2 => n + rng.below(3) as usize,
        3..=6 => 3 + rng.below(10) as usize,
        _ => 10 + rng.below(cap.saturating_sub(9).max(1) as u64) as usize,
    }
    .clamp(n, cap);
    let mut counts = vec![1usize; n];
    for _ in n..total {
        let open: Vec<usize> = (0..n).filter(|&i| counts[i] < PER_DIALOG).collect();
        counts[*rng.pick(&open)] += 1;
    }
    let p_rel = *rng.pick(&[25u64, 50, 75, 95]);
    let p_sdp = *rng.pick(&[0u64, 30, 60]);
    let hold_max = *rng.pick(&[0u64, 0, 150, 600, 1500]);
    let gap_max = *rng.pick(&[20u64, 80, 250]);
    let mut per: Vec<Vec<Provisional>> = counts
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let mut v: Vec<Provisional> = (0..c)
                .map(|_| Provisional {
                    fork: forks[i],
                    status: *rng.pick(&[180u16, 183]),
                    sdp: rng.chance(p_sdp),
                    reliable: allow_reliable && rng.chance(p_rel),
                    prack_answer_after: Duration::from_millis(rng.below(hold_max + 1)),
                })
                .collect();
            if each_reliable && !v.iter().any(|p| p.reliable) {
                let k = rng.below(c as u64) as usize;
                v[k].reliable = true;
            }
            if let Some(first) = v.iter_mut().find(|p| p.reliable) {
                first.sdp = true;
            }
            v.reverse();
            v
        })
        .collect();
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        let open: Vec<usize> = (0..n).filter(|&i| !per[i].is_empty()).collect();
        let fork = *rng.pick(&open);
        let Some(p) = per[fork].pop() else { break };
        let gap = if rng.chance(30) { 0 } else { rng.below(gap_max + 1) };
        out.push(Elem { fork, p, gap: Duration::from_millis(gap) });
    }
    out
}

/// An element in the draw log: `fork:status`, `R` reliable, `+` SDP,
/// `~ms` its PRACK's hold.
fn label(e: &Elem) -> String {
    format!(
        "{}:{}{}{}{}",
        e.fork + 1,
        e.p.status,
        if e.p.reliable { "R" } else { "" },
        if e.p.sdp { "+" } else { "" },
        if e.p.reliable && !e.p.prack_answer_after.is_zero() {
            format!("~{}", e.p.prack_answer_after.as_millis())
        } else {
            String::new()
        },
    )
}

/// Rewrite every held callee's `Ring` into a drawn train, and, with 50 %
/// chance, its next final into one sent as soon as RFC 3262 §3 allows (racing
/// the PRACKs). `allow_reliable`: the leg is offered 100rel. Returns whether
/// any element was drawn reliable.
pub fn apply(
    rng: &mut Rng,
    call: &mut ActorCall,
    max: usize,
    allow_reliable: bool,
    notes: &mut Vec<String>,
) -> bool {
    let mut any_reliable = false;
    for actor in &mut call.actors {
        if !matches!(actor.disposition, Disposition::Held) {
            continue;
        }
        let Some(at) = actor.goals.iter().position(|g| matches!(g.step, GoalStep::Ring { .. }))
        else {
            continue;
        };
        let ring = actor.goals.remove(at);
        let GoalStep::Ring { forks, reliable } = ring.step else { continue };
        let tags: Vec<Option<&'static str>> =
            if forks.is_empty() { vec![None] } else { forks.iter().copied().map(Some).collect() };
        let train = draw(rng, &tags, max, allow_reliable || reliable, reliable);
        any_reliable |= train.iter().any(|e| e.p.reliable);
        let line = train.iter().map(label).collect::<Vec<_>>().join(" ");
        let goals = train_goals(train, &ring);
        let n = goals.len();
        actor.goals.splice(at..at, goals);
        let racing = actor.goals[at + n..]
            .iter_mut()
            .find(|g| matches!(g.step, GoalStep::Answer { .. } | GoalStep::Reject { .. }))
            .is_some_and(|g| rng.chance(50) && race(g));
        let fin = if racing { "race" } else { "held" };
        notes.push(format!("train({})[{n}] {line} final={fin}", actor.role));
    }
    any_reliable
}

/// The train's goals: the first takes the ring's guard, delay and `unless`;
/// each later one waits its gap under the ring's `unless`.
fn train_goals(train: Vec<Elem>, ring: &Goal) -> Vec<Goal> {
    train
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let step = GoalStep::Provisional(e.p);
            if i == 0 {
                Goal { step, ..ring.clone() }
            } else {
                let mut g = Goal::new(Barrier::None, step).after(e.gap);
                g.unless = ring.unless.clone();
                g
            }
        })
        .collect()
}

/// Turn the held final `g` into one sent without waiting for the PRACKs.
fn race(g: &mut Goal) -> bool {
    g.step = match g.step {
        GoalStep::Answer { winner } => GoalStep::Final(HeldFinal { status: 200, winner }),
        GoalStep::Reject { status } => GoalStep::Final(HeldFinal { status, winner: None }),
        _ => return false,
    };
    true
}

/// Trains on `call`'s held callees, drawn from the call's train stream of
/// `seed`, with the caller offering 100rel when one element is reliable.
pub fn draw_trains(
    seed: u64,
    call: &mut ActorCall,
    max: usize,
    allow_reliable: bool,
    notes: &mut Vec<String>,
) {
    let mut rng = Rng::new(seed ^ TRAIN_SALT);
    if apply(&mut rng, call, max, allow_reliable, notes) {
        offer_100rel(call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_drawn_train_keeps_the_rfc_3262_bounds() {
        let forks = [Some("fk1"), Some("fk2"), Some("fk3")];
        for seed in 0..3000 {
            let mut rng = Rng::new(seed);
            let n = 1 + (seed % 3) as usize;
            let each = seed % 2 == 0;
            let t = draw(&mut rng, &forks[..n], 30, true, each);
            assert!(t.len() <= 30 && t.len() >= n);
            for (f, tag) in forks[..n].iter().enumerate() {
                let mine: Vec<&Elem> = t.iter().filter(|e| e.fork == f).collect();
                assert!(!mine.is_empty() && mine.len() <= PER_DIALOG);
                assert!(mine.iter().all(|e| e.p.fork == *tag));
                let first = mine.iter().find(|e| e.p.reliable);
                assert!(first.is_none_or(|e| e.p.sdp), "the first reliable carries SDP");
                assert!(!each || first.is_some(), "each fork has a reliable one");
            }
        }
    }

    #[test]
    fn no_element_is_reliable_unless_allowed() {
        for seed in 0..500 {
            let t = draw(&mut Rng::new(seed), &[Some("fk1"), Some("fk2")], 30, false, false);
            assert!(t.iter().all(|e| !e.p.reliable));
        }
    }
}
