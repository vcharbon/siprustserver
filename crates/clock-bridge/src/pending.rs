//! A result of real work, delivered to the paused-clock side at a virtual
//! instant.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::oneshot::{self, error::TryRecvError};
use tokio::time::Instant;

use crate::fault_log::{EnvFault, FaultLog};
use crate::mode::ClockMode;

/// Why a result was not delivered. Each is also in the side's [`FaultLog`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Undelivered {
    /// No result within the cap of real time after it fell due.
    Stall,
    /// The work ended without a result (it panicked, or the side stopped).
    Lost,
}

impl fmt::Display for Undelivered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stall => f.write_str("environment fault: external stall"),
            Self::Lost => f.write_str("environment fault: external work lost"),
        }
    }
}

/// What a held wait ended on.
enum Held<T> {
    Arrived(T),
    Lost,
    Stalled { raced: bool },
}

/// The result of one real work, started by [`RealSide::start`](crate::RealSide::start).
pub struct Pending<T> {
    label: String,
    rx: oneshot::Receiver<T>,
    stalled: Arc<AtomicBool>,
    real: Handle,
    mode: ClockMode,
    cap: Duration,
    faults: Arc<FaultLog>,
}

impl<T: Send + 'static> Pending<T> {
    pub(crate) fn new(
        label: String,
        rx: oneshot::Receiver<T>,
        stalled: Arc<AtomicBool>,
        real: Handle,
        mode: ClockMode,
        cap: Duration,
        faults: Arc<FaultLog>,
    ) -> Self {
        Self { label, rx, stalled, real, mode, cap, faults }
    }

    /// The result, at virtual instant `at` on a paused clock: virtual time
    /// runs on until `at`, then a result still on its way holds the clock up
    /// to the cap. On the real clock, the result when it arrives (`at` unused).
    pub async fn deliver_at(mut self, at: Instant) -> Result<T, Undelivered> {
        match self.mode {
            ClockMode::Real => {
                let arrived = (&mut self.rx).await;
                return arrived.map_err(|_| self.lost());
            }
            ClockMode::Paused => tokio::time::sleep_until(at).await,
        }
        match self.rx.try_recv() {
            Ok(out) => Ok(out),
            Err(TryRecvError::Closed) => Err(self.lost()),
            Err(TryRecvError::Empty) => self.hold().await,
        }
    }

    /// The result as soon as it arrives, the paused clock held meanwhile:
    /// an output the test awaits.
    pub async fn deliver_now(self) -> Result<T, Undelivered> {
        self.deliver_at(Instant::now()).await
    }

    /// Tokio holds a paused clock's auto-advance while a blocking task runs.
    /// The cap's timer is built inside the real runtime: one built on the
    /// blocking thread would read the held paused clock and never fire.
    async fn hold(self) -> Result<T, Undelivered> {
        let Self { label, mut rx, stalled, real, cap, faults, .. } = self;
        let held = tokio::task::spawn_blocking(move || {
            real.block_on(async move {
                match tokio::time::timeout(cap, &mut rx).await {
                    Ok(Ok(out)) => Held::Arrived(out),
                    Ok(Err(_)) => Held::Lost,
                    Err(_) => {
                        stalled.store(true, Ordering::Release);
                        Held::Stalled { raced: rx.try_recv().is_ok() }
                    }
                }
            })
        })
        .await
        .unwrap_or(Held::Lost);
        match held {
            Held::Arrived(out) => Ok(out),
            Held::Lost => {
                faults.record(EnvFault::Failed {
                    label,
                    detail: "the work ended without a result".into(),
                });
                Err(Undelivered::Lost)
            }
            Held::Stalled { raced } => {
                faults.record(EnvFault::Stall { label: label.clone(), cap });
                if raced {
                    // Sent between the cap and the flag: the real task saw a live receiver.
                    faults.record(EnvFault::Late { label, after: cap });
                }
                Err(Undelivered::Stall)
            }
        }
    }

    fn lost(&self) -> Undelivered {
        self.faults.record(EnvFault::Failed {
            label: self.label.clone(),
            detail: "the work ended without a result".into(),
        });
        Undelivered::Lost
    }
}
