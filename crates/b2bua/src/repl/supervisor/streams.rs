//! The streams this node pulls from a peer, as the router reads them: a
//! proxy's dead verdict on the peer is confirmed by this node's own Backup
//! flow from it (ADR-0047 D1).

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use super::super::puller::Attempt;
use super::{PullerStatus, ReplicationSupervisor};

/// What the one connect a down stream makes at a verdict found.
enum Reconnect {
    /// A frame from the peer arrived after the verdict.
    Heard,
    /// The connect failed, or the puller ended: the peer reads dead.
    Failed,
    /// The peer took the connection: its stream is open.
    Open,
}

impl ReplicationSupervisor {
    /// Whether the **Reclaim** puller is currently running (not Parked) for `peer`.
    pub fn is_running(&self, peer: &str) -> bool {
        self.inner.peers().get(peer).map(|e| e.reclaim.status_rx.is_some()).unwrap_or(false)
    }

    /// Whether this node's Backup flow from `peer` confirms a dead verdict
    /// taken at `since` on the incarnation it streams then: `false` once a
    /// frame of that incarnation arrives after `since`, the peer speaking for
    /// itself; a frame of another generation (the peer restarted) confirms it.
    ///
    /// - An open stream is waited on until it ends; one still open a tick past
    ///   the silence bound refutes the verdict, so a silent stream's idle end
    ///   at the bound always comes first.
    /// - A stream down or reconnecting at the verdict, or ended since with no
    ///   such frame, asks the puller for one connect at once, skipping the
    ///   backoff, bounded by the connect bound: a connect refused or run out
    ///   confirms it; one the peer takes opens a stream, which confirms it
    ///   when it ends with no frame, the puller's idle bound ending it.
    /// - No Backup puller running, parked or replaced meanwhile, confirms it.
    pub async fn confirms_dead(&self, peer: &str, since: Instant) -> bool {
        let flow = self.inner.peers().get(peer).and_then(|e| {
            let rx = e.backup.status_rx.clone()?;
            Some((rx, e.backup.connect_now.clone()))
        });
        let Some((mut rx, connect_now)) = flow else { return true };
        let read = Reading { since, gen: rx.borrow().last_frame_gen };
        let streaming = rx.borrow().streaming;
        if streaming {
            let bound = self.silence_bound() + SILENCE_TICK;
            match tokio::time::timeout(bound, ends_unheard(&mut rx, read)).await {
                Ok(true) => {}
                Ok(false) | Err(_) => return false,
            }
        }
        let Some(connect_now) = connect_now else { return true };
        let attempt = connect_now.ask();
        let reconnect = tokio::select! {
            attempt = attempt => match attempt {
                Ok(Attempt::Connected) => Reconnect::Open,
                Ok(Attempt::Failed) | Err(_) => Reconnect::Failed,
            },
            status = rx.wait_for(|s| s.streaming || read.decided(s)) => match status {
                Ok(status) if read.heard(&status) => Reconnect::Heard,
                Ok(status) if read.another(&status) => Reconnect::Failed,
                Ok(_) => Reconnect::Open,
                Err(_) => Reconnect::Failed,
            },
        };
        // A frame on a stream that opened and ended meanwhile still speaks for
        // the peer.
        match reconnect {
            Reconnect::Failed if read.heard(&rx.borrow()) => false,
            Reconnect::Heard => false,
            Reconnect::Failed => true,
            Reconnect::Open => ends_unheard(&mut rx, read).await,
        }
    }

    /// Whether a frame from `peer` arrived on the Backup flow after `since`.
    /// Pure read; `false` for a peer this node pulls no Backup flow from.
    pub fn heard_from_since(&self, peer: &str, since: Instant) -> bool {
        self.inner
            .peers()
            .get(peer)
            .and_then(|e| e.backup.status_rx.as_ref())
            .is_some_and(|rx| rx.borrow().last_frame_at.is_some_and(|at| at > since))
    }

    /// The longest an open stream from a peer that went silent stays open:
    /// the pullers' idle bound (ADR-0045).
    pub fn silence_bound(&self) -> Duration {
        Duration::from_millis(self.inner.config.idle_timeout_ms)
    }
}

/// How far past the silence bound an open stream must stay open to refute a
/// verdict: the puller's idle end, due at the bound at the latest, comes first.
const SILENCE_TICK: Duration = Duration::from_millis(1);

/// What a verdict reads on the Backup flow's status: frames after `since`,
/// of the incarnation generation `gen` the flow last heard before it.
#[derive(Clone, Copy)]
struct Reading {
    since: Instant,
    gen: Option<u64>,
}

impl Reading {
    /// A frame of the verdict's incarnation arrived after the verdict; with no
    /// generation heard before it, any frame is of that incarnation.
    fn heard(self, s: &PullerStatus) -> bool {
        s.last_frame_at.is_some_and(|at| at > self.since) && !self.another(s)
    }

    /// A frame of another generation arrived: the peer restarted, and the
    /// incarnation the verdict named is gone.
    fn another(self, s: &PullerStatus) -> bool {
        matches!((self.gen, s.last_frame_gen), (Some(was), Some(now)) if now != was)
    }

    /// Either settles the verdict.
    fn decided(self, s: &PullerStatus) -> bool {
        self.heard(s) || self.another(s)
    }
}

/// Resolves once the stream `rx` reports ends with no frame of the verdict's
/// incarnation (`true`, a frame of another one included) or such a frame
/// arrives (`false`); a puller that ended reads as the stream's end.
async fn ends_unheard(rx: &mut watch::Receiver<PullerStatus>, read: Reading) -> bool {
    match rx.wait_for(|s| !s.streaming || read.decided(s)).await {
        Ok(status) => !read.heard(&status),
        Err(_) => true,
    }
}
