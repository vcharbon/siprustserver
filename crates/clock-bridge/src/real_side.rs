//! The real-clock side: where every interaction with an external process
//! runs, so none of it waits on the paused clock.

use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::{Builder, Handle};
use tokio::sync::oneshot;

use crate::fault_log::{EnvFault, FaultLog};
use crate::mode::ClockMode;
use crate::pending::Pending;

/// A real-clock runtime on threads of its own, the cap on a held wait, and
/// the faults met. Dropping it stops the runtime.
pub struct RealSide {
    handle: Handle,
    mode: ClockMode,
    cap: Duration,
    faults: Arc<FaultLog>,
    _stop: oneshot::Sender<()>,
}

impl RealSide {
    /// A side for a test on `mode`, holding a late result at most `cap` of
    /// real time.
    ///
    /// # Panics
    /// When the runtime's threads cannot be started.
    pub fn new(mode: ClockMode, cap: Duration) -> Arc<Self> {
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let (stop, stopped) = oneshot::channel::<()>();
        std::thread::Builder::new()
            .name("clock-bridge".into())
            .spawn(move || {
                let rt = Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("clock-bridge-real")
                    .enable_all()
                    .build()
                    .expect("the real-clock runtime");
                let _ = handle_tx.send(rt.handle().clone());
                let _ = rt.block_on(stopped);
            })
            .expect("the real-clock thread");
        let handle = handle_rx.recv().expect("the real-clock runtime's handle");
        Arc::new(Self { handle, mode, cap, faults: Arc::default(), _stop: stop })
    }

    /// Start `work` now on the real clock; `label` names it in a fault.
    pub fn start<F>(&self, label: impl Into<String>, work: F) -> Pending<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let label = label.into();
        let (tx, rx) = oneshot::channel();
        let stalled = Arc::new(AtomicBool::new(false));
        let (late_label, late_flag, faults) = (label.clone(), stalled.clone(), self.faults.clone());
        self.handle.spawn(async move {
            let started = std::time::Instant::now();
            let out = work.await;
            if tx.send(out).is_err() && late_flag.load(std::sync::atomic::Ordering::Acquire) {
                faults.record(EnvFault::Late { label: late_label, after: started.elapsed() });
            }
        });
        Pending::new(
            label,
            rx,
            stalled,
            self.handle.clone(),
            self.mode,
            self.cap,
            self.faults.clone(),
        )
    }

    /// The clock this side delivers for.
    pub fn mode(&self) -> ClockMode {
        self.mode
    }

    /// The faults met so far, shared with every [`Pending`] it started.
    pub fn faults(&self) -> &Arc<FaultLog> {
        &self.faults
    }
}
