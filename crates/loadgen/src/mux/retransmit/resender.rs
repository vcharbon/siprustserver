//! One resender of the retransmit engine: a spawned task that re-sends one
//! datagram we sent on the SIP timer schedule until its transaction ends.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use sip_net::{ReEmitKind, SendTap, UdpEndpoint};

use super::{T1, T2, TXN_TIMEOUT};
use crate::mux::loss::DropModel;
use crate::mux::stats::MuxStats;
use crate::mux::taps::ResendCount;

/// A datagram we sent, as kept for a re-send: its shared copy, where it went,
/// and where its re-sends are counted.
#[derive(Clone)]
pub(super) struct Sent {
    pub(super) bytes: Arc<[u8]>,
    pub(super) dst: SocketAddr,
    pub(super) count: Option<ResendCount>,
}

/// Stop-control shared between a spawned resender task and the
/// [`CallTxns`](super::CallTxns) engine that owns it. The engine flips `stop`
/// (and wakes the task) when the transaction is acknowledged or the call ends;
/// the flag is authoritative (the wake is only a latency optimisation — a lost
/// `Notify` wake is caught by the post-sleep check). The transaction times out
/// at `deadline` (Timer B/F/H).
pub(super) struct ResendCtl {
    stop: AtomicBool,
    notify: tokio::sync::Notify,
    deadline: tokio::time::Instant,
}

impl ResendCtl {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
            deadline: tokio::time::Instant::now() + TXN_TIMEOUT,
        })
    }
    pub(super) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
    pub(super) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    /// Whether the transaction is still open: neither acknowledged nor past
    /// its timeout.
    pub(super) fn open(&self) -> bool {
        !self.stopped() && tokio::time::Instant::now() < self.deadline
    }
    /// Park until the transaction ends: acknowledged, or timed out.
    pub(super) async fn ended(&self) {
        loop {
            let woken = self.notify.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if self.stopped() {
                return;
            }
            tokio::select! {
                _ = woken => {}
                _ = tokio::time::sleep_until(self.deadline) => return,
            }
        }
    }
}

/// Spawn a resender: retransmit `sent` on the SIP timer schedule until
/// `ctl` is stopped or the transaction times out. `invite` selects the backoff —
/// INVITE Timer A doubles unbounded (until Timer B); non-INVITE Timer E / 2xx
/// Timer G doubles but caps at T2. Each retransmit re-applies the loss model, so a
/// retransmit can itself be dropped (the point of the robustness test).
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_resender(
    ctl: Arc<ResendCtl>,
    endpoint: Arc<dyn UdpEndpoint>,
    drop: Arc<DropModel>,
    stats: Arc<MuxStats>,
    sent: Sent,
    invite: bool,
    sendtap: Arc<OnceLock<SendTap>>,
) {
    tokio::spawn(async move {
        let Sent { bytes, dst, count } = sent;
        let mut interval = T1;
        let deadline = ctl.deadline;
        loop {
            tokio::select! {
                _ = ctl.notify.notified() => return,
                _ = tokio::time::sleep(interval) => {}
            }
            if ctl.stopped() || tokio::time::Instant::now() >= deadline {
                return;
            }
            // A re-send counts whether or not the loss model then drops it, as
            // a first transmission does.
            if let Some(count) = &count {
                count.record();
            }
            if drop.drops() {
                stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            } else {
                let _ = endpoint.as_ref().send_to(&bytes, dst).await;
                // Record the re-emission (a frame that reached the wire) on the
                // ladder — a proactive Timer A/E/G retransmit of our own frame.
                if let Some(tap) = sendtap.get() {
                    tap(&bytes, dst, ReEmitKind::Retransmit);
                }
            }
            interval = if invite {
                interval.saturating_mul(2)
            } else {
                interval.saturating_mul(2).min(T2)
            };
        }
    });
}
