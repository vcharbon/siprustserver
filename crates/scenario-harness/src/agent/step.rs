//! The fallible-core step vocabulary: [`StepError`], the [`StepSite`] a
//! scripted step was called from, and the panicking [`unwrap_step`] veneer.
//! Every agent primitive is implemented once, fallibly (`try_receive`,
//! `try_expect`, `try_send`, …); the panicking fluent methods
//! (`receive`/`expect`/`send`) are thin veneers over it.

use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::dialogs::DialogVerdict;

/// A fallible step outcome — the error type of the agent's ONE receive/expect/
/// send core. Two policies, one mechanism: a `#[tokio::test]` stops the world
/// at the first divergence (panic → the panic-time trace dump renders the wire
/// trace), while the load driver *counts* the failure and keeps going (see
/// `loadgen::class`, which matches on the VARIANT — the `Display` text is for
/// humans).
#[derive(Debug, Clone)]
pub enum StepError {
    /// Nothing the step waits for arrived in time. `awaited` names it — a
    /// method or a status, with the dialog's tags when the step knows them —
    /// and `after` is the wait bound when one applies.
    Timeout { who: String, after: Option<Duration>, awaited: String },
    /// The endpoint's receive queue closed (socket/task gone).
    QueueClosed { who: String },
    /// A datagram arrived but did not parse as SIP.
    Unparseable { who: String, detail: String },
    /// A response arrived with the wrong status code.
    WrongStatus { who: String, expected: u16, got: u16, reason: String },
    /// A request arrived with the wrong method.
    WrongMethod { who: String, expected: String, got: String },
    /// A request arrived where a response was expected (or vice-versa).
    UnexpectedKind { who: String, detail: String },
    /// Sending a datagram failed at the transport.
    Transport { who: String, detail: String },
    /// A received request failed the UA's dialog table (RFC 3261 §12.2.2).
    Dialog { who: String, verdict: Box<DialogVerdict> },
}

impl StepError {
    /// A [`StepError::Timeout`] of `who` waiting for `awaited`.
    pub fn timeout(
        who: impl Into<String>,
        after: Option<Duration>,
        awaited: impl Into<String>,
    ) -> Self {
        StepError::Timeout { who: who.into(), after, awaited: awaited.into() }
    }

    /// Name what a timed-out step waited for; any other error passes through.
    pub(crate) fn awaiting(self, awaited: impl FnOnce() -> String) -> Self {
        match self {
            StepError::Timeout { who, after, .. } => {
                StepError::Timeout { who, after, awaited: awaited() }
            }
            other => other,
        }
    }
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepError::Timeout { who, after, awaited } => match after {
                Some(after) => write!(f, "{who} timed out after {after:?} waiting for {awaited}"),
                None => write!(f, "{who} timed out waiting for {awaited}"),
            },
            StepError::QueueClosed { who } => write!(f, "{who} endpoint queue closed"),
            StepError::Unparseable { who, detail } => {
                write!(f, "{who} received an unparseable datagram: {detail}")
            }
            StepError::WrongStatus { who, expected, got, reason } => {
                write!(f, "{who} expected {expected}, got {got} {reason}")
            }
            StepError::WrongMethod { who, expected, got } => {
                write!(f, "{who} expected a {expected} request, got {got}")
            }
            StepError::UnexpectedKind { who, detail } => write!(f, "{who}: {detail}"),
            StepError::Transport { who, detail } => write!(f, "{who} send failed: {detail}"),
            StepError::Dialog { who, verdict } => write!(f, "{who}: {verdict}"),
        }
    }
}
impl std::error::Error for StepError {}

/// The ordinal source of a run's scripted steps, shared by every agent one
/// harness binds.
pub(crate) type StepCounter = std::sync::Arc<AtomicU64>;

/// Where a scripted wait (a `receive` or an `expect`) was called from: its
/// ordinal among the run's waits and the test line that called it. Captured
/// synchronously by a `#[track_caller]` veneer, before its future runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StepSite {
    ordinal: u64,
    verb: &'static str,
    at: &'static Location<'static>,
}

impl StepSite {
    /// The next step of `steps`, called from the caller's caller.
    #[track_caller]
    pub(crate) fn next(steps: &StepCounter, verb: &'static str) -> Self {
        StepSite {
            ordinal: steps.fetch_add(1, Ordering::Relaxed) + 1,
            verb,
            at: Location::caller(),
        }
    }
}

impl std::fmt::Display for StepSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "step {} ({} at {}:{})", self.ordinal, self.verb, self.at.file(), self.at.line())
    }
}

/// Unwrap a fallible-core step for the panicking (functional-test) lane: `Ok`
/// passes through, `Err` becomes `panic!("{e}")` — the same message the load
/// lane would have counted, now stopping the world instead (the panic-time
/// trace dump renders the wire trace).
#[track_caller]
pub(super) fn unwrap_step<T>(r: Result<T, StepError>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

/// [`unwrap_step`] for a scripted wait: the panic message is one line naming
/// the failure, then the step's ordinal and test line.
pub(super) fn unwrap_step_at<T>(r: Result<T, StepError>, site: StepSite) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e} at {site}"),
    }
}
