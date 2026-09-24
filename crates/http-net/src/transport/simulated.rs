//! `SimulatedHttpNetwork` — the in-memory, paused-clock workhorse.
//!
//! A routing table keyed by `SocketAddr` maps a bound address to its
//! [`HttpService`]. A [`request`](crate::HttpTransport::request) looks up the
//! service, applies the per-`dst` [`Fault`], waits the transit delay, and
//! invokes the real handler in-process. Clone shares the fabric.
//!
//! ## Transit delay >= 1 ms (the 0 -> 1 coercion)
//! Same determinism trap as `sip-net` / `repl-net`: a zero-delay
//! `tokio::time::sleep(0)` under a paused runtime races the cooperative
//! scheduler instead of parking it, so deliveries can be processed a turn late.
//! A non-zero delay makes every request park the runtime and auto-advance
//! deterministically. Coerced in [`SimulatedHttpNetwork::with_transit_delay`].
//!
//! ## Faults (dst-keyed)
//! - [`Fault::Delay`] — raise the transit delay for one destination.
//! - [`Fault::Stall`] — requests to `dst` hang (the staging "pause"): the
//!   request future awaits a resume signal and never completes on its own, so
//!   the caller's `tokio::time::timeout` fires when the harness advances.
//! - [`Fault::Resume`] — clear **any** fault on `dst` and wake stalled requests.
//! - [`Fault::Cut`] — requests to `dst` fail immediately with
//!   [`HttpError::Connect`] (connection refused).
//! - [`Fault::ErrorAfter`] — after `ms`, the request fails with
//!   [`HttpError::Io`] (ECONNRESET-style, distinct from a clean `Cut`).
//!
//! Faults are checked before the service is looked up: a faulted request is
//! one no service saw. A service answering [`HttpAnswer::Abort`] is a reset
//! of that one exchange, reported as [`HttpError::Io`] after the return
//! transit.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use super::{
    BindError, HttpAnswer, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService,
    HttpTransport,
};

/// A dst-keyed connection fault. Apply with [`SimulatedHttpNetwork::apply_fault`].
#[derive(Clone, Debug)]
pub enum Fault {
    /// Raise the per-hop transit delay (ms, coerced to `>= 1`) for `dst`.
    Delay {
        /// The destination whose delivery slows.
        dst: SocketAddr,
        /// New transit delay in milliseconds.
        ms: u64,
    },
    /// Hang requests to `dst` — they never complete until a [`Fault::Resume`]
    /// (or the caller times out). Models a black-holed / hung peer.
    Stall {
        /// The destination to stall.
        dst: SocketAddr,
    },
    /// Clear any fault on `dst` and wake stalled requests.
    Resume {
        /// The destination to resume.
        dst: SocketAddr,
    },
    /// Cut `dst` now: requests fail immediately with [`HttpError::Connect`].
    Cut {
        /// The destination to cut.
        dst: SocketAddr,
    },
    /// After `ms`, requests to `dst` fail with [`HttpError::Io`] (reset).
    ErrorAfter {
        /// The destination that resets.
        dst: SocketAddr,
        /// Delay before the error fires, in milliseconds.
        ms: u64,
    },
}

/// The live per-destination fault state (the `Delay`-carried value is folded
/// into the transit delay; `Resume` removes the entry).
#[derive(Clone)]
enum DstFault {
    Stall,
    Cut,
    ErrorAfter { ms: u64 },
}

struct Shared {
    /// Bound services, keyed by local address.
    routing: Mutex<HashMap<SocketAddr, Arc<dyn HttpService>>>,
    /// Live faults, keyed by destination.
    faults: Mutex<HashMap<SocketAddr, DstFault>>,
    /// Per-destination transit-delay override (ms, `>= 1`).
    delays: Mutex<HashMap<SocketAddr, u64>>,
    /// Default per-hop transit delay (ms, `>= 1`).
    default_delay_ms: u64,
    /// Woken on any [`Fault::Resume`]; stalled requests re-check the fault map.
    resume: Notify,
    /// Requests started and not yet finished or dropped, keyed by destination.
    in_flight: Mutex<HashMap<SocketAddr, usize>>,
}

/// Counts one request to `dst` as in flight until it finishes or its future
/// is dropped (a caller's timeout).
struct InFlight<'a> {
    shared: &'a Shared,
    dst: SocketAddr,
}

impl<'a> InFlight<'a> {
    fn start(shared: &'a Shared, dst: SocketAddr) -> Self {
        *shared.in_flight.lock().unwrap().entry(dst).or_default() += 1;
        Self { shared, dst }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut in_flight = self.shared.in_flight.lock().unwrap();
        if let Some(n) = in_flight.get_mut(&self.dst) {
            *n -= 1;
            if *n == 0 {
                in_flight.remove(&self.dst);
            }
        }
    }
}

/// The simulated HTTP fabric. Clone shares the routing table + fault state.
#[derive(Clone)]
pub struct SimulatedHttpNetwork {
    shared: Arc<Shared>,
}

impl Default for SimulatedHttpNetwork {
    fn default() -> Self {
        Self::with_transit_delay(1)
    }
}

impl SimulatedHttpNetwork {
    /// A fabric with the default 1 ms transit delay.
    pub fn new() -> Self {
        Self::default()
    }

    /// A fabric with the given per-hop transit delay (ms). `0` is coerced to
    /// `1` (see the module docs on the determinism trap).
    pub fn with_transit_delay(transit_delay_ms: u64) -> Self {
        Self {
            shared: Arc::new(Shared {
                routing: Mutex::new(HashMap::new()),
                faults: Mutex::new(HashMap::new()),
                delays: Mutex::new(HashMap::new()),
                default_delay_ms: transit_delay_ms.max(1),
                resume: Notify::new(),
                in_flight: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Inject (or clear) a fault. Live: it affects in-flight and future
    /// requests on the next fault check.
    pub fn apply_fault(&self, fault: Fault) {
        match fault {
            Fault::Delay { dst, ms } => {
                self.shared.delays.lock().unwrap().insert(dst, ms.max(1));
            }
            Fault::Stall { dst } => {
                self.shared.faults.lock().unwrap().insert(dst, DstFault::Stall);
            }
            Fault::Cut { dst } => {
                self.shared.faults.lock().unwrap().insert(dst, DstFault::Cut);
            }
            Fault::ErrorAfter { dst, ms } => {
                self.shared.faults.lock().unwrap().insert(dst, DstFault::ErrorAfter { ms });
            }
            Fault::Resume { dst } => {
                self.shared.faults.lock().unwrap().remove(&dst);
                self.shared.delays.lock().unwrap().remove(&dst);
                self.shared.resume.notify_waiters();
            }
        }
    }

    /// Requests to `dst` started and not yet answered, failed or abandoned by
    /// their caller: what a test settles on before it reads a side effect a
    /// detached request still owes.
    pub fn in_flight(&self, dst: SocketAddr) -> usize {
        self.shared.in_flight.lock().unwrap().get(&dst).copied().unwrap_or(0)
    }

    fn transit_delay(&self, dst: SocketAddr) -> u64 {
        self.shared
            .delays
            .lock()
            .unwrap()
            .get(&dst)
            .copied()
            .unwrap_or(self.shared.default_delay_ms)
    }
}

/// A bound simulated server: removing it from the routing table on drop.
struct SimServerHandle {
    addr: SocketAddr,
    shared: Arc<Shared>,
}

impl HttpServerHandle for SimServerHandle {
    fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for SimServerHandle {
    fn drop(&mut self) {
        self.shared.routing.lock().unwrap().remove(&self.addr);
    }
}

#[async_trait]
impl HttpTransport for SimulatedHttpNetwork {
    async fn serve(
        &self,
        addr: SocketAddr,
        service: Arc<dyn HttpService>,
    ) -> Result<Box<dyn HttpServerHandle>, BindError> {
        {
            let mut routing = self.shared.routing.lock().unwrap();
            if routing.contains_key(&addr) {
                return Err(BindError::AlreadyInUse(addr));
            }
            routing.insert(addr, service);
        }
        Ok(Box::new(SimServerHandle { addr, shared: self.shared.clone() }))
    }

    async fn request(&self, dst: SocketAddr, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let _in_flight = InFlight::start(&self.shared, dst);
        // Resolve faults first (re-checking after each stall wake).
        loop {
            let fault = self.shared.faults.lock().unwrap().get(&dst).cloned();
            match fault {
                Some(DstFault::Cut) => return Err(HttpError::Connect(dst)),
                Some(DstFault::ErrorAfter { ms }) => {
                    tokio::time::sleep(Duration::from_millis(ms.max(1))).await;
                    return Err(HttpError::Io { addr: dst, reason: "connection reset".into() });
                }
                Some(DstFault::Stall) => {
                    // Park until a Resume wakes us, then re-check the fault map.
                    self.shared.resume.notified().await;
                    continue;
                }
                None => break,
            }
        }

        // Look up the bound handler.
        let service = self.shared.routing.lock().unwrap().get(&dst).cloned();
        let service = match service {
            Some(s) => s,
            None => return Err(HttpError::Connect(dst)),
        };

        // Request in transit -> handler runs in-process -> response in transit.
        let delay = self.transit_delay(dst);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let answer = service.answer(req).await;
        tokio::time::sleep(Duration::from_millis(delay)).await;
        match answer {
            HttpAnswer::Response(resp) => Ok(resp),
            HttpAnswer::Abort => {
                Err(HttpError::Io { addr: dst, reason: "connection reset".into() })
            }
        }
    }
}
