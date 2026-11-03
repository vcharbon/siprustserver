//! The external face read every `PROXY_EXT_FACE_CHECK_MS` on a blocking
//! thread, its latest state published for `/readyz` and the health sampler:
//! no reader waits on the kernel on the async runtime. Before the first
//! reading completes the face reads unusable.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::task::JoinHandle;

use super::{FaceCheck, FaceLatch, FaceState};

/// How a reading is taken: the kernel's, or a test's.
pub type ReadFn = Arc<dyn Fn(&mut FaceLatch) -> FaceState + Send + Sync>;

/// The latest reading of one external face.
pub struct FaceMonitor {
    pub check: FaceCheck,
    latest: Mutex<(FaceState, FaceLatch)>,
}

impl FaceMonitor {
    /// A monitor of `check` with no reading yet.
    pub fn new(check: FaceCheck) -> Arc<Self> {
        let pending = FaceState::Unreadable("no reading yet".into());
        Arc::new(Self { check, latest: Mutex::new((pending, FaceLatch::default())) })
    }

    /// A monitor of `check` reading the running kernel every `period`, or
    /// none without a check.
    pub fn start(check: Option<FaceCheck>, period: Duration) -> Option<Arc<Self>> {
        let monitor = Self::new(check?);
        let face = monitor.check.clone();
        monitor.spawn_reading(period, Arc::new(move |latch| face.read(latch)));
        Some(monitor)
    }

    /// The latest reading.
    pub fn state(&self) -> FaceState {
        self.latest.lock().unwrap_or_else(PoisonError::into_inner).0.clone()
    }

    pub fn usable(&self) -> bool {
        self.state().is_usable()
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
        let mut latch = self.latest.lock().unwrap_or_else(PoisonError::into_inner).1;
        let reading = tokio::task::spawn_blocking(move || {
            let state = read(&mut latch);
            (state, latch)
        })
        .await;
        let mut latest = self.latest.lock().unwrap_or_else(PoisonError::into_inner);
        match reading {
            Ok(done) => *latest = done,
            Err(e) => latest.0 = FaceState::Unreadable(e.to_string()),
        }
    }
}
