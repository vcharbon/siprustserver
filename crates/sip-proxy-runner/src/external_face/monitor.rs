//! The external face read every `PROXY_EXT_FACE_CHECK_MS` on a blocking
//! thread, whether it is usable published for `/readyz` and the health
//! sampler: no reader waits on the kernel on the async runtime. Before the
//! first reading completes the face reads unusable. Each change is logged
//! once, with the reading that made it, and so is each reading held once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::{FaceCheck, FaceLatch, FaceState};

/// How a reading is taken: the kernel's, or a test's.
pub type ReadFn = Arc<dyn Fn(&mut FaceLatch) -> FaceState + Send + Sync>;

/// The latest reading of one external face.
pub struct FaceMonitor {
    pub check: FaceCheck,
    usable: AtomicBool,
    /// Carried from one reading to the next; only the reading task locks it.
    latch: Mutex<FaceLatch>,
}

impl FaceMonitor {
    /// A monitor of `check` with no reading yet.
    pub fn new(check: FaceCheck) -> Arc<Self> {
        Arc::new(Self {
            check,
            usable: AtomicBool::new(false),
            latch: Mutex::new(FaceLatch::default()),
        })
    }

    /// A monitor of `check` reading the running kernel every `period`, or
    /// none without a check.
    pub fn start(check: Option<FaceCheck>, period: Duration) -> Option<Arc<Self>> {
        let monitor = Self::new(check?);
        let face = monitor.check.clone();
        monitor.spawn_reading(period, Arc::new(move |latch| face.read(latch)));
        Some(monitor)
    }

    /// Whether the latest reading found the face usable.
    pub fn usable(&self) -> bool {
        self.usable.load(Ordering::Acquire)
    }

    /// Read with `read` every `period` (positive) on a blocking thread and
    /// publish each reading; a run that panics is restarted and counted
    /// ([`sip_proxy::supervise`]), the latch kept across runs.
    pub fn spawn_reading(self: &Arc<Self>, period: Duration, read: ReadFn) -> JoinHandle<()> {
        let monitor = self.clone();
        sip_proxy::supervise::spawn(sip_proxy::supervise::EXTERNAL_FACE_MONITOR, move || {
            let monitor = monitor.clone();
            let read = read.clone();
            async move {
                #[expect(clippy::disallowed_methods, reason = "the period is validated positive")]
                let mut ticker = tokio::time::interval(period);
                // Two readings are always a period apart, or a runtime stall
                // would make a flap shorter than the period read down twice.
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    monitor.read_once(read.clone()).await;
                }
            }
        })
    }

    async fn read_once(&self, read: ReadFn) {
        let mut carried = self.latch.lock().await;
        let mut latch = *carried;
        let reading = tokio::task::spawn_blocking(move || {
            let state = read(&mut latch);
            (state, latch)
        })
        .await;
        let state = match reading {
            Ok((state, latch)) => {
                *carried = latch;
                state
            }
            Err(e) => {
                carried.usable = false;
                FaceState::Unreadable(e.to_string())
            }
        };
        drop(carried);
        if matches!(state, FaceState::DownOnce(_) | FaceState::UnresolvedOnce) {
            tracing::warn!(
                interface = %self.check.interface,
                state = ?state,
                "external face read unusable once: a second reading withdraws it"
            );
        }
        let usable = state.is_usable();
        if self.usable.swap(usable, Ordering::AcqRel) != usable {
            let check = &self.check;
            if usable {
                tracing::info!(interface = %check.interface, "external face usable");
            } else {
                tracing::error!(
                    interface = %check.interface,
                    next_hops = ?check.next_hops,
                    state = ?state,
                    "external face unusable — /readyz fails, both faces' addresses withdrawn"
                );
            }
        }
    }
}
