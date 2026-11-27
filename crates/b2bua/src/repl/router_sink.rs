//! Where the replication pullers hand their commands to the router
//! (ADR-0011 X11 fail-back), with the router's live takeover copies a Backup
//! flow asks of each forward write it applies, `Put` or `Delete` (ADR-0047 D7),
//! and the byte budget the record bodies they hand up share
//! ([`FlushBudget`]).

use tokio::sync::mpsc;

use super::FlushBudget;
use crate::router::ReplCommand;
use crate::store::TakeoverProbe;

/// The router command sink of a wired node.
#[derive(Clone)]
pub struct RouterSink {
    /// The router's command channel.
    commands: mpsc::UnboundedSender<ReplCommand>,
    /// The router's live takeover copies: an applied forward write rides up
    /// only for one of them. `None` hands no applied write up.
    takeovers: Option<TakeoverProbe>,
    /// The bytes of bodies handed up and not yet handled, shared by every
    /// clone of the sink.
    budget: FlushBudget,
}

impl RouterSink {
    /// A sink to `commands` that reads the live takeover copies off `takeovers`.
    pub fn new(commands: mpsc::UnboundedSender<ReplCommand>, takeovers: TakeoverProbe) -> Self {
        Self { commands, takeovers: Some(takeovers), budget: FlushBudget::default() }
    }

    /// The sink, its bodies bounded by `budget`.
    pub fn with_budget(self, budget: FlushBudget) -> Self {
        Self { budget, ..self }
    }

    /// Hand `cmd` to the router, a command that carries no record body; a
    /// closed channel (shutdown) drops it.
    pub(super) fn send(&self, cmd: ReplCommand) {
        let _ = self.commands.send(cmd);
    }

    /// Hand `cmd` to the router once the record body it carries holds its
    /// bytes of the budget, waiting while the budget is spent; a closed
    /// channel (shutdown) drops it and gives the bytes back.
    pub(super) async fn hand_up(&self, mut cmd: ReplCommand) {
        if let Some(body) = cmd.body_mut() {
            self.budget.hold(body).await;
        }
        let _ = self.commands.send(cmd);
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
        Self { commands, takeovers: None, budget: FlushBudget::default() }
    }
}
