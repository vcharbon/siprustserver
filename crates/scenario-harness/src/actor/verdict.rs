//! The **scenario verdict**: a finished actor call either succeeded under one of
//! its accepted endings ([`ScenarioVerdict::Success`]) or failed at a named
//! place ([`FailedAt`]) for a [`StepError`] cause. Built once per call, at its
//! end; the success path allocates nothing.

use std::fmt;

use super::ending::Expect;
use super::state::ObservedState;
use super::CallVerdict;
use crate::StepError;

/// Where a call failed: the agent (or barrier) that failed it, that agent's
/// pending goal, and the controller barrier pending at that moment. Bounded
/// static strings and a small index, so it keys samples and metric labels.
///
/// - An actor failure names the actor's role, the goal it failed at as
///   `(kind, index)` — the pending one, or the INVITE goal when a final to the
///   establishing INVITE failed it; `None` once its goals are exhausted — and
///   the pending barrier.
/// - A barrier that never holds names that barrier as both `who` and
///   `barrier`, with no goal; `torn_down` and `settle` are barriers too.
/// - A call that settles on an ending none of its branches accepts names the
///   caller at the [`ENDING`](Self::ENDING) barrier.
/// - A scenario that refuses to build names [`BUILD`](Self::BUILD).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FailedAt {
    pub who: &'static str,
    pub goal: Option<(&'static str, u16)>,
    pub barrier: &'static str,
}

impl FailedAt {
    /// The pseudo-barrier of the verdict read after teardown and settle.
    pub const ENDING: &'static str = "ending";
    /// The scenario's build refused the call before any datagram.
    pub const BUILD: FailedAt = FailedAt::at_barrier("build");

    /// A failure of the barrier `name` itself.
    pub const fn at_barrier(name: &'static str) -> Self {
        FailedAt { who: name, goal: None, barrier: name }
    }
}

/// The compact stable key `<who>[:<kind>#<index>]@<barrier>`, e.g.
/// `alice:bye#1@established`; a barrier's own failure reads as its bare name
/// (`settle`).
impl fmt::Display for FailedAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.goal {
            None if self.who == self.barrier => f.write_str(self.barrier),
            None => write!(f, "{}@{}", self.who, self.barrier),
            Some((kind, index)) => write!(f, "{}:{kind}#{index}@{}", self.who, self.barrier),
        }
    }
}

/// The verdict of one scenario call.
#[derive(Debug)]
pub enum ScenarioVerdict {
    /// The call tore down, settled, and ended on this accepted branch.
    Success { branch: &'static str },
    /// The call failed at `at` because of `cause`.
    Failure { at: FailedAt, cause: StepError },
}

impl ScenarioVerdict {
    /// The accepted branch, `None` for a failure.
    pub fn branch(&self) -> Option<&'static str> {
        match self {
            ScenarioVerdict::Success { branch } => Some(branch),
            ScenarioVerdict::Failure { .. } => None,
        }
    }

    /// Whether the call ended on an accepted branch.
    pub fn is_success(&self) -> bool {
        matches!(self, ScenarioVerdict::Success { .. })
    }

    /// A scenario that refused to build: its guard error at [`FailedAt::BUILD`].
    pub fn build_failure(cause: StepError) -> Self {
        ScenarioVerdict::Failure { at: FailedAt::BUILD, cause }
    }
}

/// The verdict of a finished call: a runner failure as is; a settle breach at
/// the `settle` barrier; a clean call under the first branch of `expect` its
/// final state shows, or a failure at the caller's [`FailedAt::ENDING`].
pub fn verdict(
    expect: Expect,
    run: CallVerdict,
    obs: &ObservedState,
    caller: &'static str,
) -> ScenarioVerdict {
    match run {
        CallVerdict::Failed(at, cause) => ScenarioVerdict::Failure { at, cause },
        CallVerdict::Settle(_open) => ScenarioVerdict::Failure {
            at: FailedAt::at_barrier("settle"),
            cause: StepError::Timeout { who: "settle".to_string() },
        },
        CallVerdict::Ok => obs.with_snapshot(|s| match expect.branch_of(s, caller) {
            Some(branch) => ScenarioVerdict::Success { branch },
            None => {
                let at = FailedAt { who: caller, goal: None, barrier: FailedAt::ENDING };
                let cause = match s.leg_ref(caller).and_then(|l| l.first_final()) {
                    Some((got, reason)) => StepError::WrongStatus {
                        who: caller.to_string(),
                        expected: 200,
                        got,
                        reason: reason.to_string(),
                    },
                    None => StepError::UnexpectedKind {
                        who: caller.to_string(),
                        detail: "the call ended on no accepted branch".to_string(),
                    },
                };
                ScenarioVerdict::Failure { at, cause }
            }
        }),
    }
}
