//! `SimulatedHttpNetwork` — the in-memory, paused-clock workhorse.
//!
//! A routing table keyed by `SocketAddr` maps a bound address to its
//! [`HttpService`]. A [`request`](crate::HttpTransport::request) applies the
//! destination's [`Behaviour`], looks up the service, waits the transit
//! delay, and invokes the real handler in-process. Clone shares the fabric.
//!
//! ## Transit delay >= 1 ms (the 0 -> 1 coercion)
//! Same determinism trap as `sip-net` / `repl-net`: a zero-delay
//! `tokio::time::sleep(0)` under a paused runtime races the cooperative
//! scheduler instead of parking it, so deliveries can be processed a turn late.
//! A non-zero delay makes every request park the runtime and auto-advance
//! deterministically. Coerced in [`SimulatedHttpNetwork::with_transit_delay`].
//!
//! ## Behaviours: the outcomes `reqwest` produces
//! Each failure is reported as the real client classifies it
//! ([`FailureCause`](crate::failures::FailureCause)):
//! - [`Behaviour::Refuse`] — refused at once (`Refused`); so is a request to
//!   an address no service is bound at;
//! - [`Behaviour::Blackhole`] — the connect never completes: the fabric's
//!   connect timeout ([`SimulatedHttpNetwork::with_connect_timeout`], the
//!   default client's by default) ends it (`ConnectTimeout`);
//! - [`Behaviour::Silent`] — sent, never answered until a [`Fault::Resume`],
//!   so the caller's budget ends it;
//! - [`Behaviour::StallBody`] — the service answers, the headers arrive, the
//!   body never completes: the caller's budget ends it;
//! - [`Behaviour::Reset`] — reset after `after_ms` (`ConnReset`);
//! - [`Behaviour::Eof`] — closed after the request with no byte back (`Eof`);
//! - [`Behaviour::Answer`] — the service's answer.
//!
//! A destination has a standing behaviour (a [`Fault`]; `Answer` when none)
//! and a script of one-shot behaviours ([`Fault::Script`]): each request
//! takes the script's next step, and the standing behaviour once the script
//! is spent. [`Fault::Delay`] raises one destination's transit delay. A
//! faulted request is one no service saw, [`Behaviour::StallBody`] aside. A
//! service answering [`HttpAnswer::Abort`] closes the connection without a
//! response, reported as `Eof` after the return transit.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use super::{
    BindError, HttpAnswer, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService,
    HttpTransport,
};
use crate::settings::ClientSettings;

/// What a destination does with one request (see the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Behaviour {
    /// The service answers.
    Answer,
    /// The connection is refused at once.
    Refuse,
    /// The connect never completes; the connect timeout ends it.
    Blackhole,
    /// Sent and never answered until a [`Fault::Resume`].
    Silent,
    /// The service answers; the body never completes.
    StallBody,
    /// The connection is reset after `after_ms`.
    Reset {
        /// Delay before the reset, in milliseconds.
        after_ms: u64,
    },
    /// The connection is closed after the request with no byte back.
    Eof,
}

/// A dst-keyed fault. Apply with [`SimulatedHttpNetwork::apply_fault`].
#[derive(Clone, Debug)]
pub enum Fault {
    /// Raise the per-hop transit delay (ms, coerced to `>= 1`) for `dst`.
    Delay {
        /// The destination whose delivery slows.
        dst: SocketAddr,
        /// New transit delay in milliseconds.
        ms: u64,
    },
    /// Hang requests to `dst` ([`Behaviour::Silent`]): they never complete
    /// until a [`Fault::Resume`] (or the caller times out).
    Stall {
        /// The destination to stall.
        dst: SocketAddr,
    },
    /// Clear any fault and script on `dst` and wake stalled requests.
    Resume {
        /// The destination to resume.
        dst: SocketAddr,
    },
    /// Refuse requests to `dst` ([`Behaviour::Refuse`]).
    Cut {
        /// The destination to cut.
        dst: SocketAddr,
    },
    /// Reset requests to `dst` after `ms` ([`Behaviour::Reset`]).
    ErrorAfter {
        /// The destination that resets.
        dst: SocketAddr,
        /// Delay before the error fires, in milliseconds.
        ms: u64,
    },
    /// Give every request to `dst` this standing behaviour.
    Behave {
        /// The destination.
        dst: SocketAddr,
        /// What it does with each request.
        behaviour: Behaviour,
    },
    /// Give the next requests to `dst` these behaviours, one each, before
    /// the standing one.
    Script {
        /// The destination.
        dst: SocketAddr,
        /// One behaviour per request, in order.
        steps: Vec<Behaviour>,
    },
}

struct Shared {
    /// Bound services, keyed by local address.
    routing: Mutex<HashMap<SocketAddr, Arc<dyn HttpService>>>,
    /// Standing behaviours, keyed by destination.
    faults: Mutex<HashMap<SocketAddr, Behaviour>>,
    /// One-shot behaviours, keyed by destination, taken one per request.
    scripts: Mutex<HashMap<SocketAddr, VecDeque<Behaviour>>>,
    /// Per-destination transit-delay override (ms, `>= 1`).
    delays: Mutex<HashMap<SocketAddr, u64>>,
    /// Default per-hop transit delay (ms, `>= 1`).
    default_delay_ms: u64,
    /// The bound on a connect that never completes.
    connect_timeout: Duration,
    /// Woken on any [`Fault::Resume`]; stalled requests re-read the standing
    /// behaviour.
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
                scripts: Mutex::new(HashMap::new()),
                delays: Mutex::new(HashMap::new()),
                default_delay_ms: transit_delay_ms.max(1),
                connect_timeout: ClientSettings::default().connect_timeout,
                resume: Notify::new(),
                in_flight: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The same fabric with `timeout` bounding a connect that never
    /// completes ([`Behaviour::Blackhole`]). Called before the fabric is
    /// cloned.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        Arc::get_mut(&mut self.shared)
            .expect("the connect timeout is set before the fabric is shared")
            .connect_timeout = timeout;
        self
    }

    /// Inject (or clear) a fault. Live: it affects future requests, and a
    /// [`Fault::Resume`] wakes the stalled ones.
    pub fn apply_fault(&self, fault: Fault) {
        let standing = |dst, behaviour| {
            self.shared.faults.lock().unwrap().insert(dst, behaviour);
        };
        match fault {
            Fault::Delay { dst, ms } => {
                self.shared.delays.lock().unwrap().insert(dst, ms.max(1));
            }
            Fault::Stall { dst } => standing(dst, Behaviour::Silent),
            Fault::Cut { dst } => standing(dst, Behaviour::Refuse),
            Fault::ErrorAfter { dst, ms } => standing(dst, Behaviour::Reset { after_ms: ms }),
            Fault::Behave { dst, behaviour } => standing(dst, behaviour),
            Fault::Script { dst, steps } => {
                self.shared.scripts.lock().unwrap().entry(dst).or_default().extend(steps);
            }
            Fault::Resume { dst } => {
                self.shared.faults.lock().unwrap().remove(&dst);
                self.shared.scripts.lock().unwrap().remove(&dst);
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

    /// What `dst` does with the next request: the script's next step, else
    /// the standing behaviour.
    fn next_behaviour(&self, dst: SocketAddr) -> Behaviour {
        let step = self.shared.scripts.lock().unwrap().get_mut(&dst).and_then(VecDeque::pop_front);
        step.unwrap_or_else(|| self.standing(dst))
    }

    fn standing(&self, dst: SocketAddr) -> Behaviour {
        self.shared.faults.lock().unwrap().get(&dst).cloned().unwrap_or(Behaviour::Answer)
    }

    fn transit_delay(&self, dst: SocketAddr) -> Duration {
        let ms = self
            .shared
            .delays
            .lock()
            .unwrap()
            .get(&dst)
            .copied()
            .unwrap_or(self.shared.default_delay_ms);
        Duration::from_millis(ms)
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
        let mut behaviour = self.next_behaviour(dst);
        loop {
            match behaviour {
                Behaviour::Refuse => return Err(HttpError::refused(dst)),
                Behaviour::Blackhole => {
                    tokio::time::sleep(self.shared.connect_timeout).await;
                    return Err(HttpError::connect_timeout(dst));
                }
                Behaviour::Reset { after_ms } => {
                    tokio::time::sleep(Duration::from_millis(after_ms.max(1))).await;
                    return Err(HttpError::reset(dst));
                }
                Behaviour::Silent => {
                    // Park until a Resume wakes us, then re-read the
                    // standing behaviour.
                    self.shared.resume.notified().await;
                    behaviour = self.standing(dst);
                }
                Behaviour::Eof | Behaviour::StallBody | Behaviour::Answer => break,
            }
        }

        let service = self.shared.routing.lock().unwrap().get(&dst).cloned();
        let Some(service) = service else {
            return Err(HttpError::refused(dst));
        };

        // Request in transit -> handler runs in-process -> response in transit.
        let delay = self.transit_delay(dst);
        tokio::time::sleep(delay).await;
        if behaviour == Behaviour::Eof {
            tokio::time::sleep(delay).await;
            return Err(HttpError::eof(dst));
        }
        let answer = service.answer(req).await;
        tokio::time::sleep(delay).await;
        match answer {
            HttpAnswer::Response(_) if behaviour == Behaviour::StallBody => {
                std::future::pending().await
            }
            HttpAnswer::Response(resp) => Ok(resp),
            HttpAnswer::Abort => Err(HttpError::eof(dst)),
        }
    }
}
