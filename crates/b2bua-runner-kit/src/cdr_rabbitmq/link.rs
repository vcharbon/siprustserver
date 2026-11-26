//! The RabbitMQ CDR writer's link: its live session, the backoff guarding the
//! next connect, the broker cursor, and the last work of the sessions it
//! replaced. A session that ended for the broker moves the backoff and the
//! cursor; one that ended for a panic of the sink's own code moves neither.

use std::ops::{Deref, DerefMut};

use tokio::sync::MutexGuard;
use tokio::task::JoinHandle;

use super::backoff::Backoff;
use super::brokers::Brokers;
use super::session::Session;

pub(super) struct Link {
    /// `None` until the first connect, and after a session ended or retired.
    pub(super) session: Option<Session>,
    /// The confirm trackers of ended sessions, and the closes of retired
    /// ones, still resolving their publishes; joined by the writer's close.
    pub(super) retired: Vec<JoinHandle<()>>,
    pub(super) backoff: Backoff,
    pub(super) brokers: Brokers,
    /// Set when a panic unwound through a write holding the link ([`Held`]),
    /// until the writer's `after_panic` takes it.
    pub(super) panicked: bool,
}

impl Link {
    /// The link of a writer not connected yet.
    pub(super) fn new(backoff: Backoff, brokers: Brokers) -> Self {
        Self { session: None, retired: Vec::new(), backoff, brokers, panicked: false }
    }

    /// Takes the session when it ended: the next attempt starts at its broker
    /// when it delivered or ended for a panic of the sink's own code, else
    /// past it, and the backoff counts a failure from its end unless it
    /// delivered or panicked. Its tracker joins the retired work.
    pub(super) fn take_ended(&mut self) {
        let Some(ended) = self.session.take_if(|s| s.ended_at().is_some()) else {
            return;
        };
        let delivered = ended.delivered();
        let panicked = ended.ended_by_panic();
        self.brokers.ended(delivered || panicked);
        match ended.ended_at() {
            _ if delivered => self.backoff.recovered(),
            Some(ended_at) if !panicked => {
                self.backoff.failed(ended_at);
            }
            _ => {}
        }
        self.retire(ended.into_tracker());
    }

    /// Keeps `task`, a replaced session's last work, for the close to join.
    pub(super) fn retire(&mut self, task: Option<JoinHandle<()>>) {
        self.retired.retain(|task| !task.is_finished());
        self.retired.extend(task);
    }
}

/// The link held by one write. A panic unwinding through the write marks the
/// link, so the writer's `after_panic` retires the session the panic may have
/// left half-used; a write dropped by its caller, or one that returns, marks
/// nothing.
pub(super) struct Held<'a>(pub(super) MutexGuard<'a, Link>);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.panicked = true;
        }
    }
}

impl Deref for Held<'_> {
    type Target = Link;

    fn deref(&self) -> &Link {
        &self.0
    }
}

impl DerefMut for Held<'_> {
    fn deref_mut(&mut self) -> &mut Link {
        &mut self.0
    }
}
