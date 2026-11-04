//! The external face read every `PROXY_EXT_FACE_CHECK_MS` on a blocking
//! thread, whether it is usable published for `/readyz` and the health
//! sampler: no reader waits on the kernel on the async runtime. Before the
//! first reading completes the face reads unusable. Each change is logged
//! once, with the reading that made it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::{FaceCheck, FaceLatch, FaceState};

/// How a reading is taken: the kernel's, or a test's.
pub type ReadFn = Arc<dyn Fn(&mut FaceLatch) -> FaceState + Send + Sync>;

/// The latest reading of one external face.
pub struct FaceMonitor {
    pub check: FaceCheck,
    usable: AtomicBool,
    /// The latch carried from one reading to the next ([`FaceLatch`]).
    unresolved: AtomicBool,
}

impl FaceMonitor {
    /// A monitor of `check` with no reading yet.
    pub fn new(check: FaceCheck) -> Arc<Self> {
        Arc::new(Self { check, usable: AtomicBool::new(false), unresolved: AtomicBool::new(false) })
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
                loop {
                    ticker.tick().await;
                    monitor.read_once(read.clone()).await;
                }
            }
        })
    }

    async fn read_once(&self, read: ReadFn) {
        let mut latch = FaceLatch { unresolved: self.unresolved.load(Ordering::Acquire) };
        let reading = tokio::task::spawn_blocking(move || {
            let state = read(&mut latch);
            (state, latch)
        })
        .await;
        let state = match reading {
            Ok((state, latch)) => {
                self.unresolved.store(latch.unresolved, Ordering::Release);
                state
            }
            Err(e) => FaceState::Unreadable(e.to_string()),
        };
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
