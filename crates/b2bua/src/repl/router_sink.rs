//! Where the replication pullers hand their commands to the router
//! (ADR-0011 X11 fail-back), with the router's live takeover copies a Backup
//! flow asks of each forward write it applies, `Put` or `Delete` (ADR-0047 D7),
//! and the count of bulk reclaims every flow of the node handed it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::router::ReplCommand;
use crate::store::TakeoverProbe;

/// The router command sink of a wired node, one shared by all its pullers.
#[derive(Clone)]
pub struct RouterSink {
    /// The router's command channel.
    commands: mpsc::UnboundedSender<ReplCommand>,
    /// The router's live takeover copies: an applied forward write rides up
    /// only for one of them. `None` hands no applied write up.
    takeovers: Option<TakeoverProbe>,
    /// The bulk reclaims handed to the router so far, by any flow: each one
    /// materialises every `pri:{self}` body, whichever flow stored it.
    bulk_reclaims: Arc<AtomicU64>,
}

impl RouterSink {
    /// A sink to `commands` that reads the live takeover copies off `takeovers`.
    pub fn new(commands: mpsc::UnboundedSender<ReplCommand>, takeovers: TakeoverProbe) -> Self {
        Self { commands, takeovers: Some(takeovers), bulk_reclaims: Arc::default() }
    }

    /// Hand `cmd` to the router; a closed channel (shutdown) drops it.
    pub(super) fn send(&self, cmd: ReplCommand) {
        let _ = self.commands.send(cmd);
    }

    /// Hand the router a [`ReclaimAll`](ReplCommand::ReclaimAll), counted
    /// before it is sent.
    pub(super) fn reclaim_all(&self) {
        self.bulk_reclaims.fetch_add(1, Ordering::AcqRel);
        self.send(ReplCommand::ReclaimAll);
    }

    /// The bulk reclaims handed to the router so far, by any puller of the node.
    pub(super) fn bulk_reclaims(&self) -> u64 {
        self.bulk_reclaims.load(Ordering::Acquire)
    }

    /// Whether the router serves `call_ref` as a live takeover copy; `false`
    /// when no probe is wired.
    pub(super) fn serves_takeover(&self, call_ref: &str) -> bool {
        self.takeovers.as_ref().is_some_and(|probe| probe(call_ref))
    }
}

/// A sink with no takeover probe: every command but an applied forward write.
impl From<mpsc::UnboundedSender<ReplCommand>> for RouterSink {
    fn from(commands: mpsc::UnboundedSender<ReplCommand>) -> Self {
        Self { commands, takeovers: None, bulk_reclaims: Arc::default() }
    }
}
