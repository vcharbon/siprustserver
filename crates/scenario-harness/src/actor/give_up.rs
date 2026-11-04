//! The reliable-provisional give-up (RFC 3262 §3): a callee whose reliable
//! provisional went 64·T1 without its PRACK rejects the INVITE `504`, under
//! that provisional's To-tag.

use tokio::time::Instant;

use super::answer::reject_initial;
use super::react::default_react;
use super::runner::ActorState;
use super::settle::T1;
use super::shared_endpoint::Inbox;
use crate::StepError;

/// When `st` gives up on its oldest unacknowledged reliable provisional:
/// 64·T1 after sending it, while the INVITE it belongs to still awaits a
/// final. `None` when nothing is owed.
pub(super) fn due(st: &ActorState<'_>) -> Option<Instant> {
    if st.pending_prack_answer.is_none() && st.held.is_none() {
        return None;
    }
    st.unpracked.values().min().map(|sent| *sent + 64 * T1)
}

/// Park until `at` (or forever if none).
pub(super) async fn sleep(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The give-up came due: first react to whatever landed with it, so a PRACK
/// arriving at the same instant acknowledges its provisional, then reject
/// the INVITE if it is still owed.
pub(super) async fn fire(st: &mut ActorState<'_>, inbox: &mut Inbox) -> Result<(), StepError> {
    tokio::task::yield_now().await;
    while due(st).is_some() {
        let Some(m) = inbox.try_take().await else { break };
        default_react(st, m).await?;
    }
    if due(st).is_some_and(|at| at <= Instant::now()) {
        reject(st).await?;
    }
    Ok(())
}

/// Reject the held initial INVITE `504` under the To-tag of its oldest
/// unacknowledged reliable provisional (on a forked INVITE, that fork's).
async fn reject(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let Some(mut uas) = st.pending_prack_answer.take().or_else(|| st.held.take()).map(|t| *t)
    else {
        return Ok(());
    };
    if let Some((tag, _)) = st.unpracked.iter().min_by_key(|(_, sent)| **sent) {
        uas.adopt_to_tag(tag);
    }
    st.fork_answer = None;
    st.unpracked.clear();
    reject_initial(st, uas, 504).await
}
