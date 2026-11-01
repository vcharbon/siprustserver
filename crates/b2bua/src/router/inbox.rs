//! What the router loop takes next. The keepalive lane, when the transaction
//! layer runs one, comes first: an out-of-dialog OPTIONS waits for the turn
//! in progress, never behind queued events. After an OPTIONS turn, other work
//! that is ready comes first for one turn, so a steady stream of OPTIONS never
//! starves the rest: an OPTIONS waits at most for the turn in progress and,
//! when that turn was itself an OPTIONS and other work is ready, one more
//! turn. Among themselves the other sources keep `select!`'s fairness: the
//! transaction event queue, the timer fires, the re-entry channel, the
//! fail-back commands and what readmission releases. A source whose senders
//! are all gone retires.

use b2bua_sdk::event::CallEvent;
use sip_txn::{TransactionEvent, TransactionEvents};
use tokio::sync::mpsc;

use super::readmission::ReadmissionGate;
use super::{txn_event, ReplCommand, RouterCtx, Work};

/// What the loop takes next.
pub(super) enum Taken {
    /// An out-of-dialog OPTIONS from the keepalive lane.
    Keepalive(TransactionEvent),
    /// Any other work.
    Work(Work),
    /// The transaction layer is gone: the loop ends.
    Closed,
}

/// One poll's outcome, a source retiring included.
enum Next {
    Keepalive(Option<TransactionEvent>),
    Work(Work),
    TxnClosed,
    TimersClosed,
    ReentryClosed,
    ReplClosed,
}

/// The router loop's sources.
pub(super) struct Inbox {
    txn_rx: TransactionEvents,
    keepalive: Option<mpsc::Receiver<TransactionEvent>>,
    timer_rx: mpsc::UnboundedReceiver<CallEvent>,
    timers_open: bool,
    reentry_rx: mpsc::UnboundedReceiver<CallEvent>,
    reentry_open: bool,
    repl_rx: Option<mpsc::UnboundedReceiver<ReplCommand>>,
    /// Whether the last turn taken was an OPTIONS from the lane.
    after_keepalive: bool,
}

impl Inbox {
    pub(super) fn new(
        mut txn_rx: TransactionEvents,
        timer_rx: mpsc::UnboundedReceiver<CallEvent>,
        reentry_rx: mpsc::UnboundedReceiver<CallEvent>,
        repl_rx: Option<mpsc::UnboundedReceiver<ReplCommand>>,
    ) -> Self {
        let keepalive = txn_rx.take_keepalive_lane();
        Self {
            txn_rx,
            keepalive,
            timer_rx,
            timers_open: true,
            reentry_rx,
            reentry_open: true,
            repl_rx,
            after_keepalive: false,
        }
    }

    /// The next thing the loop takes.
    pub(super) async fn next(
        &mut self,
        ctx: &RouterCtx,
        readmission: &mut ReadmissionGate,
    ) -> Taken {
        loop {
            let Self {
                txn_rx,
                keepalive,
                timer_rx,
                timers_open,
                reentry_rx,
                reentry_open,
                repl_rx,
                after_keepalive,
            } = self;
            let others = next_other(
                ctx,
                txn_rx,
                (timer_rx, *timers_open),
                (reentry_rx, *reentry_open),
                repl_rx,
                readmission,
            );
            let next = if *after_keepalive {
                tokio::select! {
                    biased;
                    next = others => next,
                    ev = next_keepalive(keepalive) => Next::Keepalive(ev),
                }
            } else {
                tokio::select! {
                    biased;
                    ev = next_keepalive(keepalive) => Next::Keepalive(ev),
                    next = others => next,
                }
            };
            match next {
                Next::Keepalive(Some(ev)) => {
                    self.after_keepalive = true;
                    return Taken::Keepalive(ev);
                }
                Next::Keepalive(None) => self.keepalive = None,
                Next::Work(work) => {
                    self.after_keepalive = false;
                    return Taken::Work(work);
                }
                Next::TxnClosed => return Taken::Closed,
                Next::TimersClosed => self.timers_open = false,
                Next::ReentryClosed => self.reentry_open = false,
                Next::ReplClosed => self.repl_rx = None,
            }
        }
    }
}

/// The next work of every source but the lane, fair among them. An event
/// taken from the transaction queue has the event-queue gate read the queue:
/// a backlog that drains between two probes still restarts its hold.
async fn next_other(
    ctx: &RouterCtx,
    txn_rx: &mut TransactionEvents,
    (timer_rx, timers_open): (&mut mpsc::UnboundedReceiver<CallEvent>, bool),
    (reentry_rx, reentry_open): (&mut mpsc::UnboundedReceiver<CallEvent>, bool),
    repl_rx: &mut Option<mpsc::UnboundedReceiver<ReplCommand>>,
    readmission: &mut ReadmissionGate,
) -> Next {
    tokio::select! {
        ev = txn_rx.recv() => match ev {
            Some(ev) => {
                if let Some(gate) = ctx.readiness.queue_gate() {
                    gate.read();
                }
                let (ev, matched_client_txn) = txn_event::call_event(ev, ctx.clock.now_ms());
                Next::Work(Work::Event(ev, matched_client_txn))
            }
            None => Next::TxnClosed,
        },
        ev = timer_rx.recv(), if timers_open => match ev {
            Some(ev) => Next::Work(Work::Event(ev, false)),
            None => Next::TimersClosed,
        },
        ev = reentry_rx.recv(), if reentry_open => match ev {
            Some(ev) => Next::Work(Work::Event(ev, false)),
            None => Next::ReentryClosed,
        },
        cmd = next_fail_back(repl_rx) => match cmd {
            Some(cmd) => Next::Work(Work::Repl(cmd)),
            // No current wiring reaches it: the supervisor the router's
            // readiness holds keeps a sender.
            None => Next::ReplClosed,
        },
        cmds = readmission.released() => Next::Work(Work::Released(cmds)),
    }
}

/// The next OPTIONS of the keepalive lane; pending forever when there is no
/// lane, `None` once it closed.
async fn next_keepalive(
    lane: &mut Option<mpsc::Receiver<TransactionEvent>>,
) -> Option<TransactionEvent> {
    match lane {
        Some(lane) => lane.recv().await,
        None => std::future::pending().await,
    }
}

/// The next fail-back command; pending forever when no receiver exists.
async fn next_fail_back(
    rx: &mut Option<mpsc::UnboundedReceiver<ReplCommand>>,
) -> Option<ReplCommand> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}
