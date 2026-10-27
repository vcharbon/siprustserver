//! The [`Barrier`] a goal waits on before it fires: a guard over the observed
//! multi-endpoint state, and [`await_pred`] / [`await_paced`], the waits on one.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::state::{LegPhase, ObservedState, StateInner};
use crate::StepError;

/// A barrier guard over the observed multi-endpoint state.
#[derive(Clone)]
pub enum Barrier {
    /// Fire immediately (no guard).
    None,
    /// Fire once every named leg is at least `Confirmed` — the `established`
    /// guard (bob's REFER / alice's BYE wait on it).
    AllConfirmed(&'static [&'static str]),
    /// Fire once a named predicate over the observed state holds — the open
    /// form (the refer `merged` conjunction is `Pred`). The name is the bounded
    /// label a guard-timeout `StepError::who` carries (never free-form).
    Pred { name: &'static str, pred: Arc<dyn Fn(&StateInner) -> bool + Send + Sync> },
}

impl Barrier {
    /// A [`Barrier::Pred`] from a name + predicate (the ergonomic constructor).
    pub fn pred(
        name: &'static str,
        pred: impl Fn(&StateInner) -> bool + Send + Sync + 'static,
    ) -> Self {
        Barrier::Pred { name, pred: Arc::new(pred) }
    }

    /// A barrier that holds once `leg` has received an inbound in-dialog request
    /// of `method` (the RFC name, e.g. `"INFO"`) — the observed-fact gate that
    /// orders an origination AFTER a specific inbound request (an MRF's
    /// `INFO(EOF)` following the worker's `INFO(play)`), replacing a timed
    /// post-confirm dwell. `name` is the bounded barrier label a guard-timeout
    /// [`StepError::who`](crate::StepError) carries (never free-form).
    pub fn received(name: &'static str, leg: &'static str, method: &'static str) -> Self {
        Barrier::pred(name, move |s| s.leg_received_method(leg, method))
    }

    /// Whether the guard holds against a state snapshot.
    pub fn holds(&self, s: &StateInner) -> bool {
        match self {
            Barrier::None => true,
            Barrier::AllConfirmed(roles) => {
                roles.iter().all(|r| s.leg_at_least(r, LegPhase::Confirmed))
            }
            Barrier::Pred { pred, .. } => pred(s),
        }
    }

    /// A bounded label for the barrier-timeout `StepError::who` (never free-form).
    pub fn name(&self) -> &'static str {
        match self {
            Barrier::None => "none",
            Barrier::AllConfirmed(_) => "established",
            Barrier::Pred { name, .. } => name,
        }
    }
}

impl std::fmt::Debug for Barrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Barrier::None => f.write_str("Barrier::None"),
            Barrier::AllConfirmed(roles) => write!(f, "Barrier::AllConfirmed({roles:?})"),
            Barrier::Pred { name, .. } => write!(f, "Barrier::Pred({name:?})"),
        }
    }
}

/// Await a predicate over the observed state, re-checked on every `tick`,
/// bounded by `deadline`. `who` names the barrier for the timeout `StepError`
/// (a bounded label the case-keyer accepts — never free-form gap text).
///
/// **Lost-wake hazard**: `Notify::notified()` only registers the waiter when it
/// is first polled, not at creation, so a naive `if !pred { notified().await }`
/// loses a `notify_waiters` fired in the check→await gap. We pin the future and
/// `enable()` it — registering NOW — *before* reading the predicate, so no tick
/// between the read and the park is missed. (Under tokio auto-advance a missed
/// tick would silently leap the clock to `deadline` → a spurious timeout.)
pub async fn await_pred<F>(
    obs: &ObservedState,
    who: &'static str,
    pred: F,
    deadline: Instant,
) -> Result<(), StepError>
where
    F: Fn(&StateInner) -> bool,
{
    await_until(obs, who, pred, || deadline).await
}

/// [`await_pred`] bounded by `bound` past the later of `from` and the end of
/// the last dwell still owed ([`super::Pacing`]): a dwell scheduled while it
/// waits moves the deadline on, a dwell withdrawn moves it back.
pub async fn await_paced<F>(
    obs: &ObservedState,
    who: &'static str,
    pred: F,
    from: Instant,
    bound: Duration,
) -> Result<(), StepError>
where
    F: Fn(&StateInner) -> bool,
{
    await_until(obs, who, pred, || obs.pacing.deadline(from, bound)).await
}

/// [`await_pred`] with its deadline read again on every `tick` (a change of
/// the pacing ticks too).
async fn await_until<F>(
    obs: &ObservedState,
    who: &'static str,
    pred: F,
    deadline: impl Fn() -> Instant,
) -> Result<(), StepError>
where
    F: Fn(&StateInner) -> bool,
{
    loop {
        let notified = obs.tick.notified();
        tokio::pin!(notified);
        notified.as_mut().enable(); // register BEFORE the predicate read
        if obs.with_snapshot(&pred) {
            return Ok(());
        }
        let deadline = deadline();
        if Instant::now() >= deadline {
            return Err(StepError::timeout(who, None, format!("the call state to reach {who}")));
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}
