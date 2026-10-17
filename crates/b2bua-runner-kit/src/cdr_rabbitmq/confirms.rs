//! The confirm tracker of one session: it turns each publish's confirm into
//! exactly one `cdr_written_total` or `cdr_dropped_total`, by the writer's
//! close deadline at the latest.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use b2bua::metrics::B2buaMetrics;
use lapin::publisher_confirm::{Confirmation, PublisherConfirm};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit};
use tokio::time::{timeout, timeout_at, Instant};

use super::session::Health;

/// A publish awaiting its confirm, holding one window slot until resolved.
pub(super) struct Pending {
    pub(super) confirm: PublisherConfirm,
    pub(super) deadline: Instant,
    pub(super) _slot: OwnedSemaphorePermit,
}

/// How one confirm resolved.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Acked,
    Nacked,
    Returned,
    /// The channel or connection failed with the confirm outstanding.
    Lost(String),
    /// No confirm within the confirm bound.
    TimedOut,
    /// No confirm by the writer's close deadline.
    Closed,
    /// The session ended before the confirm arrived.
    SessionEnded,
}

/// Resolves every tracked publish, in publish order, to exactly one written
/// or dropped record. A publish still unconfirmed when the session ends, or
/// at the writer's close deadline (`close_by`), is dropped without waiting;
/// one already confirmed keeps its confirm. Exits once the session's
/// publishes stopped and every tracked publish is resolved.
pub(super) async fn track_confirms(
    mut rx: mpsc::UnboundedReceiver<Pending>,
    health: Arc<Health>,
    mut close_by: watch::Receiver<Option<Instant>>,
    metrics: B2buaMetrics,
) {
    while let Some(Pending { confirm, deadline, _slot }) = rx.recv().await {
        let outcome = if health.ended() {
            // Only a confirm already received counts; nothing is awaited.
            match timeout(Duration::ZERO, confirm).await {
                Ok(result) => outcome_of(result),
                Err(_) => Outcome::SessionEnded,
            }
        } else {
            resolve(confirm, deadline, &mut close_by).await
        };
        match outcome {
            Outcome::Acked => {
                health.delivered.store(true, Ordering::SeqCst);
                metrics.bump_cdr_written();
            }
            Outcome::Nacked => {
                metrics.bump_cdr_dropped();
                if !health.nack_logged.swap(true, Ordering::SeqCst) {
                    tracing::warn!("CDR publish nacked by the broker; nacked records are dropped");
                }
            }
            Outcome::Returned => {
                metrics.bump_cdr_dropped();
                health.end("publish returned unroutable: the queue is gone");
            }
            Outcome::Lost(e) => {
                metrics.bump_cdr_dropped();
                health.end(&format!("confirm lost: {e}"));
            }
            Outcome::TimedOut => {
                metrics.bump_cdr_dropped();
                health.end("no confirm within the confirm bound");
            }
            Outcome::Closed | Outcome::SessionEnded => metrics.bump_cdr_dropped(),
        }
    }
}

/// `confirm`'s outcome by `deadline`, or by the close deadline when one is set,
/// before or during the wait, and comes first.
async fn resolve(
    confirm: PublisherConfirm,
    deadline: Instant,
    close_by: &mut watch::Receiver<Option<Instant>>,
) -> Outcome {
    tokio::pin!(confirm);
    loop {
        let close = *close_by.borrow_and_update();
        let by = close.map_or(deadline, |close| close.min(deadline));
        tokio::select! {
            result = timeout_at(by, &mut confirm) => return match result {
                Ok(result) => outcome_of(result),
                Err(_) if by < deadline => Outcome::Closed,
                Err(_) => Outcome::TimedOut,
            },
            Ok(()) = close_by.changed() => {}
        }
    }
}

fn outcome_of(result: lapin::Result<Confirmation>) -> Outcome {
    match result {
        Ok(Confirmation::Ack(None)) => Outcome::Acked,
        Ok(Confirmation::Ack(Some(_returned))) => Outcome::Returned,
        Ok(Confirmation::Nack(_)) => Outcome::Nacked,
        // Only a channel without confirms answers this; count it lost.
        Ok(Confirmation::NotRequested) => Outcome::Lost("confirms not selected".into()),
        Err(e) => Outcome::Lost(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_ack_is_a_written_record() {
        assert_eq!(outcome_of(Ok(Confirmation::Ack(None))), Outcome::Acked);
        assert_eq!(outcome_of(Ok(Confirmation::Nack(None))), Outcome::Nacked);
        assert!(matches!(outcome_of(Ok(Confirmation::NotRequested)), Outcome::Lost(_)));
        assert!(matches!(
            outcome_of(Err(lapin::Error::InvalidChannelState(lapin::ChannelState::Closed))),
            Outcome::Lost(_)
        ));
    }
}
