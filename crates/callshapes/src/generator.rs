//! The **call-shape generator**: the cross product of seven dimensions of a
//! forked-or-not call through a B2BUA ([`Cell`]), the RFC-legality filter over
//! it ([`compatible`]), each cell's [`ShapePlan`] ([`plan`]), and the fixed
//! sample a default test lane runs ([`sample`]).
//!
//! Every cell is a full, properly terminated call on a [`Establishment::Held`]
//! callee: the nets that judge it (the RFC audit, the simulated UAs' dialog
//! tables, the SUT's own checks) turn a wrong state it reaches into a failure
//! without a rule written for that cell.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::binder::RouteBinder;
use crate::plan::{
    ByeFeed, CancelAt, DwellKnob, EarlyMethod, Establishment, ForkWinner, Forks, Party, Script,
    ShapePlan, Stage, Teardown,
};

/// The fork tags a forked cell rings under.
const FORK_TAGS: &[&str] = &["fk1", "fk2"];
/// How many early requests an early stage sends: two, so the sender's CSeq
/// sequence on the early dialog runs past the INVITE's own number.
const EARLY_COUNT: u32 = 2;
/// A short talk: well inside every SUT timer.
const SHORT_TALK: Duration = Duration::from_secs(1);
/// A talk past the keepalive interval a harness B2BUA runs with (30 s), so
/// the SUT's keepalive requests reach both parties mid-call.
const PAST_KEEPALIVE: Duration = Duration::from_secs(35);

/// Who sends the in-dialog requests of the cell (the early ones and the one
/// after the answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sender {
    Caller,
    Callee,
}

/// The requests sent on the early dialog, before the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Early {
    None,
    /// Two UPDATEs, each carrying the sender's session description as an
    /// offer.
    Update,
    /// Two bodiless INFOs.
    Info,
}

/// The request sent once the call is answered; a BYE there means the
/// teardown is the only act after the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AfterAnswer {
    Reinvite,
    Update,
    Options,
    Bye,
}

/// The early dialog that answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Winner {
    /// No fork: the one early dialog answers.
    Sole,
    /// Two forks rang; the first answers.
    First,
    /// Two forks rang; the later one answers.
    Later,
    /// Two forks rang; a fork that sent no provisional answers (RFC 3261
    /// §12.1.2, §13.2.2.4).
    Unrung,
}

/// How the call ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ending {
    CallerBye,
    CalleeBye,
    Cancel(CancelAt),
}

/// How long the parties talk before the teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Talk {
    Short,
    PastKeepalive,
}

/// One generated call shape: one value of every dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell {
    pub sender: Sender,
    pub early: Early,
    pub after: AfterAnswer,
    pub winner: Winner,
    /// Whether the callee's provisionals are reliable (RFC 3262).
    pub reliable: bool,
    pub ending: Ending,
    pub talk: Talk,
}

const SENDERS: [Sender; 2] = [Sender::Caller, Sender::Callee];
const EARLIES: [Early; 3] = [Early::None, Early::Update, Early::Info];
const AFTERS: [AfterAnswer; 4] =
    [AfterAnswer::Reinvite, AfterAnswer::Update, AfterAnswer::Options, AfterAnswer::Bye];
const WINNERS: [Winner; 4] = [Winner::Sole, Winner::First, Winner::Later, Winner::Unrung];
const RELIABLES: [bool; 2] = [false, true];
const ENDINGS: [Ending; 8] = [
    Ending::CallerBye,
    Ending::CalleeBye,
    Ending::Cancel(CancelAt::BeforeProvisional),
    Ending::Cancel(CancelAt::CrossingProvisional),
    Ending::Cancel(CancelAt::AfterProvisional),
    Ending::Cancel(CancelAt::AfterProvisionalThenEarlyBye),
    Ending::Cancel(CancelAt::CrossingFinal),
    Ending::Cancel(CancelAt::AfterFinal),
];
const TALKS: [Talk; 2] = [Talk::Short, Talk::PastKeepalive];

/// The cell where the callee sends early UPDATEs, then a re-INVITE after the
/// answer: its requests after the answer must continue the CSeq sequence its
/// early ones started (RFC 3261 §12.2.1.1).
pub const CALLEE_EARLY_UPDATES_THEN_REINVITE: Cell = Cell {
    sender: Sender::Callee,
    early: Early::Update,
    after: AfterAnswer::Reinvite,
    winner: Winner::Sole,
    reliable: true,
    ending: Ending::CallerBye,
    talk: Talk::Short,
};

/// Every cell of the cross product, in a fixed order, the RFC-illegal ones
/// included ([`compatible`] says which and why).
pub fn cells() -> Vec<Cell> {
    let mut all = Vec::new();
    for sender in SENDERS {
        for early in EARLIES {
            for after in AFTERS {
                for winner in WINNERS {
                    for reliable in RELIABLES {
                        for ending in ENDINGS {
                            for talk in TALKS {
                                all.push(Cell {
                                    sender,
                                    early,
                                    after,
                                    winner,
                                    reliable,
                                    ending,
                                    talk,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    all
}

/// Whether `cell` is a legal call, or why not.
pub fn compatible(cell: &Cell) -> Result<(), &'static str> {
    let cancelled = matches!(cell.ending, Ending::Cancel(_));
    if cancelled && cell.after != AfterAnswer::Bye {
        return Err("a cancelled call has no talk phase: its only act after an answer is the BYE");
    }
    if cancelled && cell.talk != Talk::Short {
        return Err("a cancelled call has no talk phase to hold");
    }
    if matches!(
        cell.ending,
        Ending::Cancel(CancelAt::BeforeProvisional | CancelAt::CrossingProvisional)
    ) && cell.early != Early::None
    {
        return Err("a CANCEL before the provisional leaves no early dialog to send on");
    }
    if cell.early == Early::Update && !cell.reliable {
        return Err("RFC 3311 §5.1: an early UPDATE offer needs the INVITE's answer, \
                    which only a reliable provisional carries");
    }
    if cell.early != Early::None && cell.sender == Sender::Callee && !cell.reliable {
        return Err("RFC 3262: the callee learns the caller holds its early dialog \
                    only from the PRACK");
    }
    if cell.early == Early::None && cell.after == AfterAnswer::Bye && cell.sender == Sender::Callee
    {
        return Err("no in-dialog request is sent, so the sender is moot (the caller's cell \
                    stands for it)");
    }
    Ok(())
}

/// The [`ShapePlan`] of a [`compatible`] cell, routed through `binder`.
pub fn plan(cell: &Cell, binder: Arc<dyn RouteBinder>) -> ShapePlan {
    let forks = match cell.winner {
        Winner::Sole => Forks::Sole,
        Winner::First => Forks::Forked { tags: FORK_TAGS, winner: ForkWinner::Tag(FORK_TAGS[0]) },
        Winner::Later => Forks::Forked { tags: FORK_TAGS, winner: ForkWinner::Tag(FORK_TAGS[1]) },
        Winner::Unrung => Forks::Forked { tags: FORK_TAGS, winner: ForkWinner::Unrung },
    };
    let by = match cell.sender {
        Sender::Caller => Party::Caller,
        Sender::Callee => Party::Callee,
    };
    let mut stages = Vec::new();
    match cell.early {
        Early::None => {}
        Early::Update => {
            stages.push(Stage::Early { by, method: EarlyMethod::Update, count: EARLY_COUNT })
        }
        Early::Info => {
            stages.push(Stage::Early { by, method: EarlyMethod::Info, count: EARLY_COUNT })
        }
    }
    let script = match cell.after {
        AfterAnswer::Reinvite => Some(Script::Reinvite { n: 1 }),
        AfterAnswer::Update => Some(Script::UpdatePostConnect),
        AfterAnswer::Options => Some(Script::Ping),
        AfterAnswer::Bye => None,
    };
    if let Some(script) = script {
        stages.push(match by {
            Party::Caller => Stage::Script(script),
            Party::Callee => Stage::Callee(script),
        });
    }
    let talk = DwellKnob::Fixed(match cell.talk {
        Talk::Short => SHORT_TALK,
        Talk::PastKeepalive => PAST_KEEPALIVE,
    });
    let teardown = match cell.ending {
        Ending::CallerBye => Teardown::CallerBye { after: talk, feed: ByeFeed::NoFeed },
        Ending::CalleeBye => Teardown::CalleeBye { after: talk },
        Ending::Cancel(at) => Teardown::Cancel(at),
    };
    ShapePlan {
        id: "generated_cell",
        binder,
        establish: Establishment::Held { forks, reliable: cell.reliable },
        stages,
        teardown,
        ringing_gate: false,
        stamp_connected: false,
    }
}

/// The fixed sample a default lane runs: the [`CALLEE_EARLY_UPDATES_THEN_REINVITE`]
/// cell first, then compatible cells picked greedily (the earliest in
/// [`cells`] order among the best) until every triple of dimension values
/// some compatible cell holds is held by a picked cell, so every pair and
/// every value appears too.
pub fn sample() -> Vec<Cell> {
    let legal: Vec<Cell> = cells().into_iter().filter(|c| compatible(c).is_ok()).collect();
    let mut uncovered: HashSet<Triple> = legal.iter().flat_map(triples).collect();
    let mut picked = Vec::new();
    let mut pick = |cell: Cell, uncovered: &mut HashSet<Triple>| {
        for t in triples(&cell) {
            uncovered.remove(&t);
        }
        picked.push(cell);
    };
    pick(CALLEE_EARLY_UPDATES_THEN_REINVITE, &mut uncovered);
    while !uncovered.is_empty() {
        let gain = |c: &Cell| triples(c).filter(|t| uncovered.contains(t)).count();
        let (_, best) = legal
            .iter()
            .enumerate()
            .map(|(i, c)| ((gain(c), std::cmp::Reverse(i)), *c))
            .max_by_key(|(key, _)| *key)
            .expect("a compatible cell holds each uncovered triple");
        pick(best, &mut uncovered);
    }
    picked
}

/// Three `(dimension, value index)` coordinates of one cell, by dimension.
type Triple = [(usize, usize); 3];

/// Each dimension's value as `(dimension, value index)`.
fn coordinates(c: &Cell) -> [(usize, usize); 7] {
    let at = |i: Option<usize>| i.expect("every value is listed");
    [
        (0, at(SENDERS.iter().position(|v| *v == c.sender))),
        (1, at(EARLIES.iter().position(|v| *v == c.early))),
        (2, at(AFTERS.iter().position(|v| *v == c.after))),
        (3, at(WINNERS.iter().position(|v| *v == c.winner))),
        (4, at(RELIABLES.iter().position(|v| *v == c.reliable))),
        (5, at(ENDINGS.iter().position(|v| *v == c.ending))),
        (6, at(TALKS.iter().position(|v| *v == c.talk))),
    ]
}

/// The triples of dimension values `c` holds, dimensions ascending.
fn triples(c: &Cell) -> impl Iterator<Item = Triple> {
    let co = coordinates(c);
    let n = co.len();
    (0..n).flat_map(move |i| {
        ((i + 1)..n).flat_map(move |j| ((j + 1)..n).map(move |k| [co[i], co[j], co[k]]))
    })
}

impl fmt::Display for Cell {
    /// Every dimension's value, in one line: `by=… early=… then=… win=…
    /// 1xx=… end=… talk=…`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sender = match self.sender {
            Sender::Caller => "caller",
            Sender::Callee => "callee",
        };
        let early = match self.early {
            Early::None => "none",
            Early::Update => "update",
            Early::Info => "info",
        };
        let after = match self.after {
            AfterAnswer::Reinvite => "reinvite",
            AfterAnswer::Update => "update",
            AfterAnswer::Options => "options",
            AfterAnswer::Bye => "bye",
        };
        let winner = match self.winner {
            Winner::Sole => "sole",
            Winner::First => "first-fork",
            Winner::Later => "later-fork",
            Winner::Unrung => "unrung-fork",
        };
        let reliable = if self.reliable { "reliable" } else { "plain" };
        let ending = match self.ending {
            Ending::CallerBye => "caller-bye",
            Ending::CalleeBye => "callee-bye",
            Ending::Cancel(CancelAt::BeforeProvisional) => "cancel-before-1xx",
            Ending::Cancel(CancelAt::CrossingProvisional) => "cancel-crossing-1xx",
            Ending::Cancel(CancelAt::AfterProvisional) => "cancel-after-1xx",
            Ending::Cancel(CancelAt::AfterProvisionalThenEarlyBye) => "cancel-then-early-bye",
            Ending::Cancel(CancelAt::CrossingFinal) => "cancel-crossing-final",
            Ending::Cancel(CancelAt::AfterFinal) => "cancel-after-final",
        };
        let talk = match self.talk {
            Talk::Short => "short",
            Talk::PastKeepalive => "past-keepalive",
        };
        write!(
            f,
            "by={sender} early={early} then={after} win={winner} 1xx={reliable} \
             end={ending} talk={talk}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::default_binder;

    /// The cross product, and every compatible cell compiles to a valid plan.
    #[test]
    fn every_compatible_cell_is_a_valid_plan() {
        let all = cells();
        assert_eq!(all.len(), 2 * 3 * 4 * 4 * 2 * 8 * 2);
        for cell in all.iter().filter(|c| compatible(c).is_ok()) {
            plan(cell, default_binder()).validate().unwrap_or_else(|e| panic!("{cell}: {e:?}"));
        }
    }

    /// The sample starts with the reality-check cell, holds every value of
    /// every dimension, and only legal cells.
    #[test]
    fn the_sample_covers_every_value() {
        let sample = sample();
        assert_eq!(sample[0], CALLEE_EARLY_UPDATES_THEN_REINVITE);
        assert!(sample.iter().all(|c| compatible(c).is_ok()));
        for dim in 0..7 {
            let legal: HashSet<usize> = cells()
                .iter()
                .filter(|c| compatible(c).is_ok())
                .map(|c| coordinates(c)[dim].1)
                .collect();
            let seen: HashSet<usize> = sample.iter().map(|c| coordinates(c)[dim].1).collect();
            assert_eq!(seen, legal, "dimension {dim}");
        }
    }

    /// An early UPDATE on plain provisionals is refused, with the RFC's word.
    #[test]
    fn an_unreliable_early_update_is_refused() {
        let cell = Cell { reliable: false, ..CALLEE_EARLY_UPDATES_THEN_REINVITE };
        assert!(compatible(&cell).unwrap_err().starts_with("RFC 3311 §5.1"));
    }
}
