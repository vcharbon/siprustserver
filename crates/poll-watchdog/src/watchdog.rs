//! The watchdog: the poll hooks a runtime is built with, the thread that
//! reads every registered thread's slot and CPU time, and the exit a stuck
//! poll takes.

use std::cell::OnceCell;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::watch;
use tokio::task::Id;

use crate::criterion::{judge, Reading, Verdict, Watch};
use crate::poison::{locked, SLOTS};
use crate::schedstat;
use crate::slot::Slot;

/// The exit code of a process a stuck poll ends.
pub const EXIT_CODE: u8 = 70;

/// The longest interval between two reads of the slots.
pub const READ_PERIOD: Duration = Duration::from_secs(1);

/// How long the process has to exit after a stuck poll before the watchdog
/// ends it, until the host states its own bound.
pub const DEFAULT_EXIT_AFTER: Duration = Duration::from_secs(1);

/// The call a task serves, when the host knows it.
pub type CallOf = Arc<dyn Fn(Id) -> Option<String> + Send + Sync>;

/// A poll the watchdog read stuck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stuck {
    /// The task polled; `None` when the thread left the poll as it was read.
    pub task: Option<Id>,
    /// The kernel id of the thread the poll holds.
    pub thread: u32,
    /// The CPU the poll spent since the watchdog first saw it.
    pub cpu: Duration,
    /// The call the task serves, when the host named one.
    pub call_ref: Option<String>,
}

/// The process's poll watchdog. Clone-cheap.
#[derive(Clone)]
pub struct Watchdog {
    shared: Arc<Shared>,
}

struct Shared {
    /// Tells this watchdog's slots from another's on a thread.
    id: u64,
    slots: Mutex<Vec<Arc<Slot>>>,
    next_serial: AtomicU64,
    started: AtomicBool,
    call_of: OnceLock<CallOf>,
    exit_after_ms: AtomicU64,
    stuck: watch::Sender<Option<Stuck>>,
}

/// The slot the calling thread registered with a watchdog.
struct Registered {
    owner: u64,
    slot: Arc<Slot>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.slot.mark_gone();
    }
}

thread_local! {
    static REGISTERED: OnceCell<Registered> = const { OnceCell::new() };
}

static PROCESS: OnceLock<Watchdog> = OnceLock::new();

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl Watchdog {
    /// A watchdog no thread registered with, not started.
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let (stuck, _) = watch::channel(None);
        let exit_after_ms = u64::try_from(DEFAULT_EXIT_AFTER.as_millis()).unwrap_or(u64::MAX);
        Self {
            shared: Arc::new(Shared {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                slots: Mutex::new(Vec::new()),
                next_serial: AtomicU64::new(0),
                started: AtomicBool::new(false),
                call_of: OnceLock::new(),
                exit_after_ms: AtomicU64::new(exit_after_ms),
                stuck,
            }),
        }
    }

    /// The one watchdog of the process, which its runtime is built with.
    pub fn process() -> &'static Watchdog {
        PROCESS.get_or_init(Watchdog::new)
    }

    /// Install the poll hooks on `builder`: a thread registers on its first
    /// poll, so a thread that never polls a task, a blocking-pool thread
    /// among them, never does.
    pub fn install<'b>(&self, builder: &'b mut Builder) -> &'b mut Builder {
        let before = self.clone();
        builder
            .on_before_task_poll(move |meta| before.entering(meta.id()))
            .on_after_task_poll(|_| leaving())
    }

    /// Start the watchdog thread with `threshold`: a poll that spends more
    /// CPU than it on one thread is stuck. `Err` when this thread's CPU time
    /// cannot be read or the thread does not start: the host refuses to
    /// boot. A watchdog started already keeps its first threshold.
    pub fn start(&self, threshold: Duration) -> io::Result<()> {
        schedstat::cpu(schedstat::own_tid()?)?;
        if self.shared.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let shared = self.shared.clone();
        let started = std::thread::Builder::new()
            .name("poll-watchdog".into())
            .spawn(move || watch_slots(&shared, threshold));
        if started.is_err() {
            self.shared.started.store(false, Ordering::SeqCst);
        }
        started.map(drop)
    }

    /// Name the call each task serves in a stuck poll's report; the first
    /// naming holds.
    pub fn call_of(&self, call_of: CallOf) {
        let _ = self.shared.call_of.set(call_of);
    }

    /// How long the process has to exit after a stuck poll before the
    /// watchdog thread ends it with [`EXIT_CODE`].
    pub fn exit_after(&self, bound: Duration) {
        let ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        self.shared.exit_after_ms.store(ms, Ordering::Relaxed);
    }

    /// Resolves with the first poll read stuck; never when none is.
    pub async fn stuck(&self) -> Stuck {
        let mut rx = self.shared.stuck.subscribe();
        loop {
            if let Some(stuck) = rx.borrow_and_update().clone() {
                return stuck;
            }
            if rx.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    /// The kernel ids of the live threads registered with this watchdog.
    pub fn registered_threads(&self) -> Vec<u32> {
        let slots = locked(&self.shared.slots, SLOTS);
        slots.iter().filter(|s| !s.is_gone()).map(|s| s.tid).collect()
    }

    /// The calling thread starts a poll of `task`, registering on its first.
    fn entering(&self, task: Id) {
        REGISTERED.with(|cell| {
            if cell.get().is_none() {
                if let Some(slot) = self.register() {
                    let _ = cell.set(Registered { owner: self.shared.id, slot });
                }
            }
            if let Some(registered) = cell.get().filter(|r| r.owner == self.shared.id) {
                registered.slot.enter(task);
            }
        });
    }

    /// A slot for the calling thread, `None` when its id cannot be read.
    fn register(&self) -> Option<Arc<Slot>> {
        let tid = match schedstat::own_tid() {
            Ok(tid) => tid,
            Err(e) => {
                tracing::error!(error = %e, "poll watchdog: a runtime thread's id does not read; it goes unwatched");
                return None;
            }
        };
        let serial = self.shared.next_serial.fetch_add(1, Ordering::Relaxed);
        let slot = Arc::new(Slot::new(serial, tid));
        locked(&self.shared.slots, SLOTS).push(slot.clone());
        Some(slot)
    }
}

/// The calling thread's poll returned.
fn leaving() {
    REGISTERED.with(|cell| {
        if let Some(registered) = cell.get() {
            registered.slot.leave();
        }
    });
}

/// The interval between reads: [`READ_PERIOD`], or half the threshold when
/// that is shorter, so a poll is read stuck within twice its threshold.
fn period_for(threshold: Duration) -> Duration {
    READ_PERIOD.min(threshold / 2).max(Duration::from_millis(1))
}

/// The watchdog thread: read every live slot each period, until a poll
/// reads stuck.
fn watch_slots(shared: &Shared, threshold: Duration) {
    let period = period_for(threshold);
    let mut watches: Vec<(u64, Watch)> = Vec::new();
    loop {
        std::thread::sleep(period);
        let slots = live_slots(shared);
        let mut next = Vec::with_capacity(watches.len());
        for slot in &slots {
            let Some(reading) = read(slot) else { continue };
            let previous = watches.iter().find(|(s, _)| *s == slot.serial).map(|(_, w)| *w);
            match judge(previous, reading, threshold) {
                Verdict::Idle => {}
                Verdict::Running(watch) => next.push((slot.serial, watch)),
                Verdict::Stuck { spent } => exit_stuck(shared, slot, reading.seq, spent),
            }
        }
        watches = next;
    }
}

/// The registered slots, those of exited threads dropped.
fn live_slots(shared: &Shared) -> Vec<Arc<Slot>> {
    let mut slots = locked(&shared.slots, SLOTS);
    slots.retain(|s| !s.is_gone());
    slots.clone()
}

/// The slot's sequence number and its thread's CPU time; `None` when the
/// thread is gone.
fn read(slot: &Slot) -> Option<Reading> {
    let seq = slot.seq();
    let cpu = schedstat::cpu(slot.tid).ok()?;
    Some(Reading { seq, cpu })
}

/// Report the stuck poll, publish it, and end the process once the host's
/// exit bound has passed.
fn exit_stuck(shared: &Shared, slot: &Slot, seq: u64, spent: Duration) -> ! {
    let task = slot.task_at(seq);
    let call_ref = task.and_then(|task| shared.call_of.get().and_then(|call_of| call_of(task)));
    let cpu_ms = u64::try_from(spent.as_millis()).unwrap_or(u64::MAX);
    tracing::error!(
        task = task.map(|t| t.to_string()),
        thread = slot.tid,
        cpu_ms,
        call_ref = call_ref.as_deref(),
        "a poll is stuck on the CPU: exiting"
    );
    shared.stuck.send_replace(Some(Stuck { task, thread: slot.tid, cpu: spent, call_ref }));
    let exit_after_ms = shared.exit_after_ms.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(exit_after_ms));
    eprintln!("poll watchdog: the process did not exit within {exit_after_ms} ms of a stuck poll: exiting now");
    std::process::exit(i32::from(EXIT_CODE))
}

#[cfg(test)]
mod tests;
