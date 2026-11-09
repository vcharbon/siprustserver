//! One runtime thread's poll state, shared with the watchdog thread: a
//! sequence number odd inside a poll and even between polls, the task the
//! poll runs, and the thread's kernel id. A thread gets its slot on its first
//! poll and marks it gone when it exits.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_utils::atomic::AtomicCell;
use tokio::task::Id;

pub(crate) struct Slot {
    /// Unique among the slots of one watchdog.
    pub(crate) serial: u64,
    pub(crate) tid: u32,
    seq: AtomicU64,
    task: AtomicCell<Option<Id>>,
    gone: AtomicBool,
}

impl Slot {
    pub(crate) fn new(serial: u64, tid: u32) -> Self {
        Self {
            serial,
            tid,
            seq: AtomicU64::new(0),
            task: AtomicCell::new(None),
            gone: AtomicBool::new(false),
        }
    }

    /// The thread starts polling `task`: the task is stored before the
    /// sequence number turns odd.
    pub(crate) fn enter(&self, task: Id) {
        self.task.store(Some(task));
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// The thread's poll returned: the sequence number turns even. A slot
    /// already even (the thread got its slot inside a poll) stays as it is.
    pub(crate) fn leave(&self) {
        if !self.seq.load(Ordering::Relaxed).is_multiple_of(2) {
            self.seq.fetch_add(1, Ordering::Release);
        }
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// The task of the poll at sequence number `seq`: `None` once the thread
    /// has moved past it.
    pub(crate) fn task_at(&self, seq: u64) -> Option<Id> {
        let task = self.task.load();
        (self.seq() == seq).then_some(task).flatten()
    }

    pub(crate) fn mark_gone(&self) {
        self.gone.store(true, Ordering::Release);
    }

    pub(crate) fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }
}
