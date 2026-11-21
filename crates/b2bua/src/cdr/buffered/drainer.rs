//! The buffered writer's drainer: one run at a time takes the queue and
//! writes each record into the inner sink; the queue, the close deadline and
//! the closed signal are shared state a later run takes over.
//!
//! - A panic of the inner write is caught: its record is counted dropped
//!   once, through the sink ([`CdrWriter::dropped`]), the panic counted under
//!   [`CDR_DRAINER`] and said at most once per [`LINE_INTERVAL`], and the
//!   sink told to drop its partial state ([`CdrWriter::after_panic`]).
//! - Any other panic ends the run. It is counted under [`CDR_DRAINER`], and a
//!   new run starts [`RESTART_DELAY`] later, at once when a close begins
//!   meanwhile, by telling the sink to drop its partial state; records keep
//!   queuing until then. A run's own code panics only after the record it
//!   holds is counted.
//! - Once a close began, a run that panics is made again at once, and that
//!   run writes the queue until the deadline. A second run that panics in
//!   the close is not made again: the rest of the queue is counted dropped,
//!   the sink closed and the close reported, so a close ends by its deadline.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures_util::FutureExt;
use sip_clock::wall::millis_u64;
use task_restart::RESTART_DELAY;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::{timeout, timeout_at, Instant};

use call::Call;

use super::Dropped;
use crate::abort_on_drop::AbortOnDrop;
use crate::cdr::{CdrDrop, CdrWriter, LineRate, LINE_INTERVAL};
use crate::supervise::{CDR_DRAINER, RESTARTS};

/// The drainer's half of the buffer: it holds no sender, so the queue ends
/// once every writer is gone.
#[derive(Clone)]
pub(super) struct Drainer {
    pub(super) inner: Arc<dyn CdrWriter>,
    pub(super) dropped: Dropped,
    /// The submit queue's receiving end, held by one run at a time.
    pub(super) queue: Arc<Mutex<mpsc::Receiver<(Call, i64)>>>,
    pub(super) close_by: Arc<watch::Sender<Option<Instant>>>,
    pub(super) closed: Arc<watch::Sender<bool>>,
    /// The caught write panics, every run's, and the rate of their line.
    pub(super) write_panics: Arc<LineRate>,
}

impl Drainer {
    /// Runs the drainer until a run returns: after a close, or once every
    /// writer is gone. A run that panics is counted and followed by another
    /// (the module's rules). Aborting this task aborts the run in flight.
    pub(super) async fn supervise(self) {
        let mut after_panic = false;
        let mut panicked_in_close = false;
        loop {
            let mut run = AbortOnDrop(observe::spawn_keeping_node(self.clone().run(after_panic)));
            match (&mut run.0).await {
                Err(e) if e.is_panic() => RESTARTS.restarted(CDR_DRAINER),
                Ok(()) | Err(_) => return,
            }
            after_panic = true;
            let closing = *self.close_by.borrow();
            match closing {
                Some(deadline) if panicked_in_close => return self.give_up(deadline).await,
                Some(_) => panicked_in_close = true,
                None => {
                    let mut close_by = self.close_by.subscribe();
                    let _ = timeout(RESTART_DELAY, close_by.wait_for(Option::is_some)).await;
                }
            }
        }
    }

    /// Writes each queued record until a close deadline is set, then closes
    /// the queue, writes what is left until the deadline (each record past it
    /// is dropped), closes `inner` and reports `closed`. Ends without a close
    /// once every writer is gone. A run following a panicked one first tells
    /// `inner` to drop its partial state.
    async fn run(self, after_panic: bool) {
        let mut rx = self.queue.lock().await;
        let mut close_by = self.close_by.subscribe();
        if after_panic {
            let _ = until_close(self.inner.after_panic(), &mut close_by).await;
        }
        let deadline = loop {
            let by = *close_by.borrow_and_update();
            if let Some(deadline) = by {
                break deadline;
            }
            tokio::select! {
                biased;
                Ok(()) = close_by.changed() => {}
                record = rx.recv() => match record {
                    Some((call, at)) => self.write(&call, at, &mut close_by).await,
                    None => return,
                },
            }
        };
        rx.close();
        while let Some((call, at)) = rx.recv().await {
            if Instant::now() < deadline {
                self.write(&call, at, &mut close_by).await;
            } else {
                self.dropped.one(CdrDrop::CloseDeadline);
            }
        }
        drop(rx);
        self.close_inner(deadline).await;
        self.closed.send_replace(true);
    }

    /// Writes one record; once a close deadline is set the write runs until
    /// that deadline at most, and a write cut there is dropped. A write that
    /// panics counts its record dropped, then `inner` drops its partial state
    /// within the same bound.
    async fn write(&self, call: &Call, at: i64, close_by: &mut watch::Receiver<Option<Instant>>) {
        let handed_at = Instant::now();
        match until_close(caught(self.inner.write(call, at)), close_by).await {
            Some(Ok(())) => {}
            Some(Err(Panicked)) => {
                self.write_panicked(call, handed_at);
                let _ = until_close(self.inner.after_panic(), close_by).await;
            }
            None => self.unfinished(CdrDrop::CloseDeadline, handed_at),
        }
    }

    /// The one count of a record handed to `inner` at `handed_at` whose write
    /// counted nothing, through `inner`'s own drops.
    fn unfinished(&self, why: CdrDrop, handed_at: Instant) {
        self.dropped.local();
        self.inner.dropped(why, handed_at, &self.dropped.metrics);
    }

    /// The one count of a record whose write panicked: dropped for
    /// [`CdrDrop::Panic`], the panic counted under [`CDR_DRAINER`], and the
    /// line saying it due at most once per [`LINE_INTERVAL`].
    fn write_panicked(&self, call: &Call, handed_at: Instant) {
        self.unfinished(CdrDrop::Panic, handed_at);
        RESTARTS.survived(CDR_DRAINER);
        if let Some(panics) = self.write_panics.counted() {
            tracing::error!(
                call_ref = %call.call_ref,
                panics,
                interval_ms = millis_u64(LINE_INTERVAL),
                "the CDR sink panicked writing records since the last such line, each \
                 counted dropped; said at most once per interval"
            );
        }
    }

    /// Closes `inner` by `deadline`; a close that panics is counted under
    /// [`CDR_DRAINER`] and over.
    async fn close_inner(&self, deadline: Instant) {
        if caught(self.inner.close(deadline)).await.is_err() {
            RESTARTS.survived(CDR_DRAINER);
            tracing::error!("the CDR sink panicked closing; its close is over");
        }
    }

    /// The close after a second run panicked in it: every record still
    /// queued, or sent by `deadline` on a slot reserved before the queue
    /// closed, is counted dropped for [`CdrDrop::Panic`], `inner` closed by
    /// `deadline`, and `closed` reported.
    async fn give_up(&self, deadline: Instant) {
        let mut rx = self.queue.lock().await;
        rx.close();
        let reserved = async {
            while rx.recv().await.is_some() {
                self.dropped.one(CdrDrop::Panic);
            }
        };
        if timeout_at(deadline, reserved).await.is_err() {
            while rx.try_recv().is_ok() {
                self.dropped.one(CdrDrop::Panic);
            }
        }
        drop(rx);
        self.close_inner(deadline).await;
        self.closed.send_replace(true);
    }
}

/// A future that panicked while polled.
struct Panicked;

/// `work`'s output, or [`Panicked`]. The sink's state after a panic is
/// dropped through [`CdrWriter::after_panic`] before its next record, and the
/// drainer's own state is counters, so observing a panic across this
/// boundary leaves nothing half-done in use.
async fn caught<T>(work: impl Future<Output = T>) -> Result<T, Panicked> {
    AssertUnwindSafe(work).catch_unwind().await.map_err(|_| Panicked)
}

/// `work`'s output, or `None` when a close deadline, set before or during
/// it, passes first.
async fn until_close<T>(
    work: impl Future<Output = T>,
    close_by: &mut watch::Receiver<Option<Instant>>,
) -> Option<T> {
    tokio::pin!(work);
    loop {
        let by = *close_by.borrow_and_update();
        if let Some(deadline) = by {
            return timeout_at(deadline, &mut work).await.ok();
        }
        tokio::select! {
            out = &mut work => return Some(out),
            Ok(()) = close_by.changed() => {}
        }
    }
}
