//! The admission ladder as data: [`LADDER`] is the order a new INVITE meets
//! the rungs in, each [`Rung`] carries its input, and [`judge`] decides it
//! for one [`Class`]. The router judges its rungs in that order
//! ([`first_refusal`]).

use sip_message::emergency::is_emergency_request;
use sip_message::SipRequest;

use crate::capacity::CapacityReading;
use crate::new_calls::Refusal;

/// The class a new INVITE is judged in: normal, emergency (an RFC 4412
/// emergency priority), or in a dialog (a To-tag; judged by the backlog
/// alone).
pub use sip_txn::InviteClass as Class;

/// The class of `req`: the one classification every rung reads.
pub fn class_of(req: &SipRequest) -> Class {
    if req.to().tag().is_some() {
        Class::InDialog
    } else if is_emergency_request(req) {
        Class::Emergency
    } else {
        Class::Normal
    }
}

/// One rung of the ladder, without its input. The brake is judged on
/// arrival, before the datagram is queued; the backlog in the transaction
/// layer, before any transaction exists, by the
/// [`DeferredBound`](sip_txn::DeferredBound) of
/// [`deferred_bound`](super::deferred_bound); the other four at router
/// ingress ([`first_refusal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Brake,
    Backlog,
    Capacity,
    Shed,
    PanicElu,
    Bucket,
}

/// The order a new INVITE meets the rungs in.
pub const LADDER: [Step; 6] =
    [Step::Brake, Step::Backlog, Step::Capacity, Step::Shed, Step::PanicElu, Step::Bucket];

/// A rung [`judge`] decides, with the input it is judged on.
#[derive(Debug, Clone, Copy)]
pub enum Rung {
    /// The inbound queue's live depth against the brake's threshold.
    Brake { depth: usize, threshold: usize },
    /// The live calls, transactions and sampled RSS against their ceilings.
    Capacity(CapacityReading),
    /// Whether the call has no queue and the live queues reach the threshold
    /// of the INVITE's dispatch row: the global cap, less the new-call
    /// headroom for a normal INVITE ([`crate::dispatch::class`]).
    Shed { at_threshold: bool },
    /// The worker's EWMA-ELU against the panic backstop.
    PanicElu { elu: f64, threshold: f64 },
    /// Seconds until the CPS bucket holds a token: 0 when it holds one.
    Bucket { wait_sec: u32 },
}

/// A rung's refusal: why, and the least `Retry-After` base it states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    pub reason: Refusal,
    /// Seconds before which the refused call cannot be admitted: the
    /// bucket's time to a token, 0 elsewhere. The answer's `Retry-After`
    /// spreads from the larger of this and the configured base.
    pub not_before_sec: u32,
}

impl Refused {
    const fn now(reason: Refusal) -> Self {
        Self { reason, not_before_sec: 0 }
    }
}

/// What a rung decided for one INVITE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Admit,
    Refuse(Refused),
}

/// Decide `rung` for an INVITE of `class`. An emergency INVITE is refused
/// only at its own capacity ceilings and the shed's full cap; an in-dialog
/// INVITE is refused only by the backlog, which the transaction layer judges.
pub fn judge(rung: Rung, class: Class) -> Verdict {
    let refused = match (rung, class) {
        (_, Class::InDialog) => None,
        (Rung::Brake { depth, threshold }, Class::Normal) => {
            (depth >= threshold).then_some(Refused::now(Refusal::IngressBrake))
        }
        (Rung::Capacity(reading), _) => reading.refusal(class).map(Refused::now),
        (Rung::Shed { at_threshold }, _) => at_threshold.then_some(Refused::now(Refusal::CapShed)),
        (Rung::PanicElu { elu, threshold }, Class::Normal) => {
            (elu > threshold).then_some(Refused::now(Refusal::PanicElu))
        }
        (Rung::Bucket { wait_sec }, Class::Normal) => (wait_sec > 0)
            .then_some(Refused { reason: Refusal::BucketEmpty, not_before_sec: wait_sec }),
        (Rung::Brake { .. } | Rung::PanicElu { .. } | Rung::Bucket { .. }, Class::Emergency) => {
            None
        }
    };
    refused.map_or(Verdict::Admit, Verdict::Refuse)
}

/// The inputs of the router's rungs, each read only when the ladder reaches
/// its rung.
pub trait RouterReadings {
    /// The capacity rung's reading.
    fn capacity(&self) -> CapacityReading;
    /// Whether the shed rung finds the live queues at the INVITE's threshold.
    fn at_threshold(&self) -> bool;
    /// The EWMA-ELU and the panic backstop's threshold.
    fn panic_elu(&self) -> (f64, f64);
    /// Seconds until the CPS bucket holds a token.
    fn token_wait_sec(&self) -> u32;
}

/// The first of the router's rungs, in [`LADDER`] order, that refuses an
/// INVITE of `class`; the brake and the backlog were judged before it
/// arrived.
pub fn first_refusal(class: Class, readings: &impl RouterReadings) -> Option<Refused> {
    LADDER.into_iter().find_map(|step| {
        let rung = match step {
            Step::Brake | Step::Backlog => return None,
            Step::Capacity => Rung::Capacity(readings.capacity()),
            Step::Shed => Rung::Shed { at_threshold: readings.at_threshold() },
            Step::PanicElu => {
                let (elu, threshold) = readings.panic_elu();
                Rung::PanicElu { elu, threshold }
            }
            Step::Bucket => Rung::Bucket { wait_sec: readings.token_wait_sec() },
        };
        match judge(rung, class) {
            Verdict::Admit => None,
            Verdict::Refuse(refused) => Some(refused),
        }
    })
}
