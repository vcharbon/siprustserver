//! sip-retransmit — the one home for SIP retransmission pacing (ADR-0029).
//!
//! A **ladder** is the whole sequence of scheduled re-sends of one message; a
//! **rung** is one step on it (rung 0 is the original send, rung 1 the first
//! re-send). A [`Schedule`] answers what a class of message waits before each
//! rung and when the ladder gives up; a [`Ladder`] is the cursor that walks it.
//!
//! Both kinds of retransmission ride the same schedules, and the distinction
//! between them is one of ownership, not of pacing:
//!
//! - **Transaction ladders** (Timer A/E, Timer G, the CANCEL sub-ladder) are
//!   driven inside `sip-txn`'s owner task and are invisible to its consumers.
//! - **Dialog-level ladders** (RFC 3261 §13.3.1.4, RFC 3262 §3) belong to an
//!   obligation replicated with the call, and reach a B2BUA rule only as their
//!   give-up.
//!
//! Pure by construction: no clock, no timers, no I/O. The owner arms its own
//! timer from the [`Duration`] this crate hands it.
//!
//! ## What cannot be expressed here
//!
//! [`Schedule::exact`] and [`Schedule::once`] sit behind the `authored`
//! feature, which the SUT crates never enable. A production ladder is
//! therefore always one an RFC prescribes: a schedule the wire measured is a
//! thing a replay or a harness states, never a thing the stack invents.
//!
//! The `serde` feature derives `Serialize`/`Deserialize` on [`Class`] only, for
//! the consumer that replicates a ladder's class beside its rung.

#![forbid(unsafe_code)]

use std::time::Duration;

pub mod timers;

use timers::{T1, T2, TIMER_B, TIMER_F, TIMER_H};

/// The doubling ceiling on the exponent, so a ladder nothing ever closed
/// cannot overflow the shift. 2^20 · T1 is ~6 days: far past every give-up
/// below, so the clamp is unobservable.
const MAX_DOUBLINGS: u32 = 20;

/// The retransmission class of a message: what an RFC obliges its emitter to
/// re-send, and how often.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Class {
    /// An INVITE client transaction's request, RFC 3261 §17.1.1.2 (Timer A):
    /// T1 doubling with NO ceiling, bounded by Timer B. The first provisional
    /// stops it outright — a state fact its owner applies, not a schedule.
    /// The class's bound is Timer B; an owner may tighten it through
    /// [`Schedule::with_give_up`] (deployment policy, not an RFC value).
    InviteClient,
    /// A non-INVITE client transaction's request in `Trying`, §17.1.2.2
    /// (Timer E): T1 doubling, capped at T2, bounded by Timer F.
    NonInviteClient,
    /// The same request once the transaction reaches `Proceeding`: §17.1.2.2
    /// re-arms at exactly T2 from there on. A distinct class rather than a
    /// caller-side clamp, so a ladder that changes pace mid-flight does so by
    /// [`Ladder::retarget`] and keeps its elapsed time honest.
    NonInviteProceeding,
    /// A CANCEL parked on its INVITE's branch (ADR-0028 X4): a non-INVITE
    /// request, so §17.1.2.2's pacing, bounded at 64·T1 — and the ceiling gives
    /// up on the CANCEL alone, never on the INVITE it answers.
    CancelClient,
    /// An INVITE server transaction's non-2xx final, §17.2.1 (Timer G): T1
    /// doubling capped at T2, until the ACK or Timer H.
    InviteServerFinal,
    /// A 2xx answering an INVITE, §13.3.1.4: the UAS re-sends it until the ACK
    /// or 64·T1 (Timer L). Capped at T2 — an ACK is triggered by receipt of the
    /// 2xx, so the peer's answer is one round trip away.
    Final2xx,
    /// A reliable provisional, RFC 3262 §3: T1 doubling with NO ceiling, until
    /// the PRACK or 64·T1. §3 states the difference from a 2xx itself — a PRACK
    /// is a request of its own, sent independently of the 1xx that owes it.
    ReliableProvisional,
}

impl Class {
    /// Every class, for a consumer that enumerates the family.
    pub const ALL: [Class; 7] = [
        Class::InviteClient,
        Class::NonInviteClient,
        Class::NonInviteProceeding,
        Class::CancelClient,
        Class::InviteServerFinal,
        Class::Final2xx,
        Class::ReliableProvisional,
    ];

    /// The stable name a counter labels this class by (`ladder="…"`), the
    /// same on both planes: the transaction layer and the dialog-level ladders
    /// name a class here and nowhere else.
    pub const fn as_str(self) -> &'static str {
        match self {
            Class::InviteClient => "invite-client",
            Class::NonInviteClient => "non-invite-client",
            Class::NonInviteProceeding => "non-invite-proceeding",
            Class::CancelClient => "cancel-client",
            Class::InviteServerFinal => "invite-server-final",
            Class::Final2xx => "final-2xx",
            Class::ReliableProvisional => "reliable-provisional",
        }
    }

    /// The ceiling on a rung's interval, where the class has one.
    const fn cap(self) -> Option<Duration> {
        match self {
            Class::InviteClient | Class::ReliableProvisional => None,
            Class::NonInviteClient
            | Class::NonInviteProceeding
            | Class::CancelClient
            | Class::InviteServerFinal
            | Class::Final2xx => Some(timers::ms(T2)),
        }
    }

    /// How long the ladder runs before the class gives up, measured from the
    /// original send. An owner with a bound of its own states it through
    /// [`Schedule::with_give_up`].
    const fn give_up(self) -> Duration {
        match self {
            Class::InviteClient => timers::ms(TIMER_B),
            Class::NonInviteClient | Class::NonInviteProceeding => timers::ms(TIMER_F),
            // 64·T1, as Timer F and Timer L are — stated per class so raising
            // one never silently moves the others.
            Class::CancelClient | Class::Final2xx | Class::ReliableProvisional => {
                timers::ms(64 * T1)
            }
            Class::InviteServerFinal => timers::ms(TIMER_H),
        }
    }

    /// The wait before rung `n` (counting from 1).
    fn interval(self, n: u32) -> Duration {
        if let Class::NonInviteProceeding = self {
            return timers::ms(T2);
        }
        let doubled = timers::ms(T1) * 2u32.saturating_pow(n.saturating_sub(1).min(MAX_DOUBLINGS));
        match self.cap() {
            Some(cap) => doubled.min(cap),
            None => doubled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pacing {
    Rfc(Class),
    #[cfg(feature = "authored")]
    Exact(Vec<Duration>),
    #[cfg(feature = "authored")]
    Once,
}

/// What a ladder waits before each rung, and when it gives up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pacing: Pacing,
    give_up: Duration,
}

impl Schedule {
    /// The schedule an RFC prescribes for `class`. The only constructor the SUT
    /// can reach.
    pub fn rfc(class: Class) -> Schedule {
        Schedule {
            pacing: Pacing::Rfc(class),
            give_up: class.give_up(),
        }
    }

    /// Replace this schedule's give-up with the owner's own bound, which may
    /// sit either side of the class's: a deployment may configure an INVITE
    /// bound below Timer B, and RFC 3261 lets the tighter deadline own the
    /// give-up.
    pub fn with_give_up(self, give_up: Duration) -> Schedule {
        Schedule { give_up, ..self }
    }

    /// This schedule bounded by `deadline` or by the class, whichever comes
    /// first: a policy deadline may cut a ladder short, never carry it past
    /// what the RFC prescribes (a 2xx ceases at Timer L, RFC 3261 §13.3.1.4,
    /// however long a deployment waits before it acts on the silence).
    pub fn tightened_to(self, deadline: Duration) -> Schedule {
        let give_up = self.give_up.min(deadline);
        Schedule { give_up, ..self }
    }

    /// The class this schedule paces, where it paces one.
    pub fn class(&self) -> Option<Class> {
        match self.pacing {
            Pacing::Rfc(c) => Some(c),
            #[cfg(feature = "authored")]
            _ => None,
        }
    }

    /// The bound, measured from the original send.
    pub fn give_up_after(&self) -> Duration {
        self.give_up
    }

    /// The wait before rung `n`, counting from 1; `None` where the schedule has
    /// no such rung.
    pub fn interval(&self, n: u32) -> Option<Duration> {
        if n == 0 {
            return None;
        }
        match &self.pacing {
            Pacing::Rfc(class) => Some(class.interval(n)),
            #[cfg(feature = "authored")]
            Pacing::Exact(gaps) => gaps.get(n as usize - 1).or_else(|| gaps.last()).copied(),
            #[cfg(feature = "authored")]
            Pacing::Once => None,
        }
    }

    /// The elapsed time at rung `n`: the sum of every wait up to and including
    /// it.
    pub fn elapsed_at(&self, n: u32) -> Duration {
        (1..=n).filter_map(|k| self.interval(k)).sum()
    }
}

/// The schedules an RFC does not prescribe. Behind the `authored` feature: a
/// capture whose platform paced its own ladder, and a harness simulating an
/// endpoint that does not behave.
#[cfg(feature = "authored")]
impl Schedule {
    /// One stated interval per rung, in order — the gaps the wire measured. A
    /// count past the stated list repeats the last gap, so a ladder longer than
    /// its author described paces steadily rather than falling back onto a
    /// class it never chose.
    ///
    /// `give_up` defaults to the sum of the stated gaps: an authored ladder
    /// that says nothing about its bound ends when its list ends.
    pub fn exact(intervals: &[Duration], give_up: Option<Duration>) -> Schedule {
        let gaps: Vec<Duration> = intervals.to_vec();
        let bound = give_up.unwrap_or_else(|| gaps.iter().copied().sum());
        Schedule {
            pacing: Pacing::Exact(gaps),
            give_up: bound,
        }
    }

    /// Emitted once, no rung ever. Distinct from every RFC class: an ACK and an
    /// unreliable provisional ride no timer of their own, and saying so is not
    /// the same as saying "pace by the RFC".
    pub fn once() -> Schedule {
        Schedule {
            pacing: Pacing::Once,
            give_up: Duration::ZERO,
        }
    }
}

/// A cursor walking one [`Schedule`]: which rung the ladder stands on, and how
/// long it has been running. A ladder that never [`retarget`](Self::retarget)s
/// rebuilds whole from its rung alone ([`Ladder::at_rung`]), which is what a
/// replicated body stores (ADR-0029 X2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    schedule: Schedule,
    rung: u32,
    elapsed: Duration,
}

impl Ladder {
    /// Arm the first rung. Returns the ladder and the wait before it — armed
    /// unconditionally, because a class that retransmits at all owes its first
    /// re-send.
    pub fn armed(schedule: Schedule) -> Option<(Ladder, Duration)> {
        let first = schedule.interval(1)?;
        Some((
            Ladder {
                schedule,
                rung: 1,
                elapsed: first,
            },
            first,
        ))
    }

    /// Rebuild a ladder standing on `rung`, deriving the elapsed time from the
    /// schedule. The form a replicated rung index is restored through.
    pub fn at_rung(schedule: Schedule, rung: u32) -> Ladder {
        let elapsed = schedule.elapsed_at(rung);
        Ladder {
            schedule,
            rung,
            elapsed,
        }
    }

    /// Step to the next rung and return the wait before it, or `None` when that
    /// rung would land at or past the give-up — the ladder is over and whatever
    /// the give-up owes takes it from there.
    pub fn advance(&mut self) -> Option<Duration> {
        let next = self.schedule.interval(self.rung + 1)?;
        let landing = self.elapsed + next;
        if landing >= self.schedule.give_up {
            return None;
        }
        self.rung += 1;
        self.elapsed = landing;
        Some(next)
    }

    /// Re-pace onto `class`, keeping both the elapsed total and the bound: the
    /// ladder changes speed, never its deadline.
    pub fn retarget(&mut self, class: Class) {
        self.schedule = Schedule::rfc(class).with_give_up(self.schedule.give_up);
    }

    /// The class pacing the ladder at this rung, where it paces one — after a
    /// [`retarget`](Self::retarget), the class it was re-paced onto.
    pub fn class(&self) -> Option<Class> {
        self.schedule.class()
    }

    /// Which rung the ladder stands on. Rung 0 is the original send.
    pub fn rung(&self) -> u32 {
        self.rung
    }

    /// How long the ladder has been running, at its current rung.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// The bound, measured from the original send.
    pub fn give_up_after(&self) -> Duration {
        self.schedule.give_up_after()
    }
}

#[cfg(test)]
mod tests;
