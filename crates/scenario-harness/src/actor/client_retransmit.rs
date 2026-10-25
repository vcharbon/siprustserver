//! The requests a leg sent and still awaits an answer to, retransmitted over
//! the unreliable transport as their client transactions do (RFC 3261
//! §17.1.1.2, §17.1.2.2): a re-INVITE on Timer A — after T1, doubling —
//! until any response or Timer B; a PRACK on Timer E — after T1, doubling up
//! to T2 — until its final or Timer F. Both timers end the retransmission at
//! 64·T1.

use std::time::Duration;

use sip_message::SipResponse;
use tokio::time::Instant;

use super::runner::ActorState;
use super::settle::T1;
use crate::{InDialogTxn, StepError};

/// RFC 3261 T2: the cap of a non-INVITE request's retransmit interval.
const T2: Duration = Duration::from_secs(4);

/// The request a retransmission repeats.
pub(super) enum Retransmitted {
    /// A PRACK, on its own transaction.
    Prack(InDialogTxn),
    /// A re-INVITE, whose transaction the leg keeps among its sent re-INVITEs.
    Reinvite,
}

/// One request awaiting its answer.
pub(super) struct Pending {
    /// The request's CSeq number: its responses name it.
    cseq: u32,
    what: Retransmitted,
    /// When the timer next fires.
    next: Instant,
    /// The interval the timer last waited.
    interval: Duration,
    /// When Timer B or F ends the transaction.
    until: Instant,
}

impl Pending {
    /// `what`, numbered `cseq`, sent at `now`.
    pub(super) fn sent(cseq: u32, what: Retransmitted, now: Instant) -> Self {
        Pending { cseq, what, next: now + T1, interval: T1, until: now + 64 * T1 }
    }
}

/// Retransmit the PRACK numbered `cseq`, sent on `txn` at `now`, until its
/// final.
pub(super) fn prack_sent(st: &mut ActorState<'_>, cseq: u32, txn: InDialogTxn, now: Instant) {
    st.retransmits.push(Pending::sent(cseq, Retransmitted::Prack(txn), now));
}

/// Park until the earliest timer is due (or forever if none).
pub(super) async fn wait(pending: &[Pending]) {
    match pending.iter().map(|p| p.next).min() {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Retransmit every request whose timer is due and drop those whose
/// transaction timed out.
pub(super) async fn fire(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let now = Instant::now();
    st.retransmits.retain(|p| p.until > now);
    for i in 0..st.retransmits.len() {
        if st.retransmits[i].next > now {
            continue;
        }
        let p = &st.retransmits[i];
        match &p.what {
            Retransmitted::Prack(txn) => txn.try_retransmit().await?,
            Retransmitted::Reinvite => {
                if let Some(txn) = st.sent_reinvite_txns.get(&p.cseq) {
                    txn.try_retransmit().await?;
                }
            }
        }
        let p = &mut st.retransmits[i];
        p.interval = match p.what {
            Retransmitted::Prack(_) => (p.interval * 2).min(T2),
            Retransmitted::Reinvite => p.interval * 2,
        };
        p.next = now + p.interval;
    }
    Ok(())
}

/// A response ends the retransmission of the request it answers: a re-INVITE
/// on any response, a PRACK on its final.
pub(super) fn settle(st: &mut ActorState<'_>, resp: &SipResponse) {
    let cseq = resp.cseq().seq();
    let method = resp.cseq().method().as_str();
    let final_ = resp.status() >= 200;
    st.retransmits.retain(|p| {
        p.cseq != cseq
            || match p.what {
                Retransmitted::Prack(_) => method != "PRACK" || !final_,
                Retransmitted::Reinvite => method != "INVITE",
            }
    });
}
