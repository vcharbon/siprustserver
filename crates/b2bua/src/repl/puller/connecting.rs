//! The puller's connect: resolve the peer fresh (ADR-0012 D3) and connect
//! within the connect bound, racing the bootstrap hard deadline and the
//! cancel signal; a connect asked for at once ([`super::ConnectNow`]) while one is in
//! flight starts afresh, and the outcome answers every request held.

use std::time::Duration;

use repl_net::transport::ReplicationConnection;
use tokio::sync::watch;

use super::{cancelled, Asks, Attempt, Puller, RunOutcome};

impl Puller {
    /// Resolve the peer's address (fresh) and open a connection within
    /// `connect_timeout_ms`. `None` if the address is unresolvable right now or
    /// the connect fails or runs out of time — the caller treats each as a
    /// failed connect (→ Backoff, then retry/re-resolve).
    async fn try_connect(&self) -> Option<Box<dyn ReplicationConnection>> {
        let addr = self.resolve.resolve(&self.peer).await?;
        let bound = Duration::from_millis(self.config.connect_timeout_ms);
        match tokio::time::timeout(bound, self.network.connect(addr)).await {
            Ok(conn) => conn.ok(),
            Err(_) => {
                tracing::debug!(peer = %self.peer.ordinal, %addr, "repl connect timed out");
                None
            }
        }
    }

    /// Connect, or the outcome that ends this run: the bootstrap hard deadline
    /// tripped mid-connect (best-effort complete, then back off), the connect
    /// failed, or the puller was cancelled. A connected stream reads
    /// `streaming` before the requests held hear [`Attempt::Connected`].
    pub(super) async fn connect(
        &self,
        cancel: &mut watch::Receiver<bool>,
        deadline: &mut Option<tokio::time::Instant>,
        asks: &mut Asks,
    ) -> Result<Box<dyn ReplicationConnection>, RunOutcome> {
        // Racing the hard deadline lets a hung connect to an unreachable or
        // stalled peer still let the node go bootstrap-complete. The connect
        // that answers a request starts after it.
        asks.take_queued();
        let conn = loop {
            match *deadline {
                Some(at) => tokio::select! {
                    c = self.try_connect() => break c,
                    _ = tokio::time::sleep_until(at) => {
                        // Hard timer tripped mid-connect: best-effort complete for
                        // readiness, then back off + retry (now warm → no hard
                        // timer). A connect asked for keeps waiting on the next.
                        self.mark_bootstrap_complete(0, "hard timeout while connecting");
                        *deadline = None;
                        return Err(RunOutcome::ConnectFailed);
                    }
                    () = cancelled(cancel) => return Err(RunOutcome::Cancelled),
                    () = asks.next() => {}
                },
                None => tokio::select! {
                    c = self.try_connect() => break c,
                    () = cancelled(cancel) => return Err(RunOutcome::Cancelled),
                    () = asks.next() => {}
                },
            }
            self.note_state("connecting", "a connect is asked for at once: connecting afresh");
        };
        let Some(conn) = conn else {
            asks.answer(Attempt::Failed);
            return Err(RunOutcome::ConnectFailed);
        };
        // The peer is reached (readiness no longer reads it unreachable), streaming.
        self.status_tx.send_modify(|s| {
            s.ever_connected = true;
            s.streaming = true;
        });
        asks.take_queued();
        asks.answer(Attempt::Connected);
        Ok(conn)
    }
}
