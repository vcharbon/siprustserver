//! [`BackendClient`] — the client of one HTTP backend a server calls on its
//! request path (ADR-0043).
//!
//! One per backend, over any [`HttpTransport`]: the transport's own client
//! for the backend's [`ResolvedTarget`] ([`HttpTransport::client`]), renewed
//! every rotation period; at most [`ClientSettings::max_in_flight`] requests
//! in flight; and a circuit breaker under the window rule ([`WindowBreaker`])
//! that a probe closes. One attempt ([`BackendClient::attempt`]) waits for a
//! slot and its answer inside its budget, capped by the settings' ceiling, so
//! no request is ever unbounded; [`BackendClient::send_with_retry`] sends a
//! failed attempt once more under the generic rule.
//!
//! While the breaker is open, an attempt sends nothing and fails at once;
//! what that means for the caller (fail the call, let it through) is the
//! caller's policy. Opening the breaker forgets the target's addresses.

mod breaker;
mod config;
pub mod metrics;
mod probe;
mod retry;
mod rotation;
mod slots;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub use breaker::{BreakerConfig, Sent, WindowBreaker};
pub use config::BackendConfig;
pub use retry::Budgets;

use crate::resolved_target::ResolvedTarget;
use crate::resolver::NameResolver;
use crate::settings::ClientSettings;
use crate::transport::{HttpClient, HttpError, HttpRequest, HttpResponse, HttpTransport};

/// Why an attempt got no response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttemptError {
    /// The breaker is open: nothing was sent.
    #[error("the backend's breaker is open")]
    BreakerOpen,
    /// No in-flight slot freed within the budget: nothing was sent.
    #[error("no in-flight slot freed within the budget")]
    Saturated,
    /// Sent; no response within the budget.
    #[error("no response within the budget")]
    Timeout,
    /// The transport failed.
    #[error(transparent)]
    Transport(HttpError),
}

impl AttemptError {
    /// Whether the generic rule sends the request once more after it: a
    /// connection-level failure or a timeout, never a request left unsent.
    pub fn is_retryable(&self) -> bool {
        match self {
            AttemptError::Timeout => true,
            AttemptError::Transport(e) => e.cause.is_retryable(),
            AttemptError::BreakerOpen | AttemptError::Saturated => false,
        }
    }
}

/// What a probe saw: the response, or why it got none.
pub type ProbeReport = Arc<dyn Fn(Result<&HttpResponse, &AttemptError>) + Send + Sync>;

/// One backend's client. Clone shares it.
#[derive(Clone)]
pub struct BackendClient {
    inner: Arc<Inner>,
}

struct Inner {
    target: Arc<ResolvedTarget>,
    /// The target as configured: the metric label and the log name.
    peer: String,
    ceiling: Duration,
    rotation: rotation::Rotation,
    slots: slots::Slots,
    config: BreakerConfig,
    breaker: Mutex<WindowBreaker>,
    open: watch::Sender<bool>,
    probe: Mutex<Option<JoinHandle<()>>>,
    report: Mutex<Option<ProbeReport>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(probe) = self.probe.get_mut().unwrap_or_else(PoisonError::into_inner).take() {
            probe.abort();
        }
    }
}

impl BackendClient {
    /// The client of `target` over `transport`, holding its connections as
    /// `settings` state, its breaker under `breaker`.
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        target: Arc<ResolvedTarget>,
        settings: ClientSettings,
        breaker: BreakerConfig,
    ) -> Self {
        let peer = target.to_string();
        let (open, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                slots: slots::Slots::new(settings.max_in_flight, peer.clone()),
                ceiling: settings.ceiling,
                rotation: rotation::Rotation::new(transport, target.clone(), settings),
                target,
                peer,
                breaker: Mutex::new(WindowBreaker::new(&breaker)),
                config: breaker,
                open,
                probe: Mutex::new(None),
                report: Mutex::new(None),
            }),
        }
    }

    /// The client of the backend named `name` (`host:port`, or a socket
    /// address) over `transport`, tuned by `config`, its name resolved by
    /// `resolver` under `config`'s lookup bounds.
    pub fn configured(
        transport: Arc<dyn HttpTransport>,
        name: &str,
        resolver: Arc<dyn NameResolver>,
        config: &BackendConfig,
    ) -> Self {
        let target = ResolvedTarget::name_with_lookup(name, resolver, config.lookup);
        Self::new(transport, Arc::new(target), config.client.clone(), config.breaker.clone())
    }

    /// The backend's target.
    pub fn target(&self) -> &Arc<ResolvedTarget> {
        &self.inner.target
    }

    /// Whether the breaker is open.
    pub fn is_open(&self) -> bool {
        *self.inner.open.borrow()
    }

    /// The breaker's state as it changes: `true` while open.
    pub fn breaker_state(&self) -> watch::Receiver<bool> {
        self.inner.open.subscribe()
    }

    /// Open the breaker now, whatever the window holds; the probe closes it.
    pub fn trip(&self) {
        let tripped = self.inner.lock_breaker().trip();
        if tripped {
            self.inner.opened();
        }
    }

    /// Look the target's name up now, off the caller, and open the breaker
    /// when the lookup finds no address: until a probe is answered, a request
    /// fails at once instead of waiting for a lookup of its own.
    pub fn trip_unless_resolved(&self) {
        let inner = Arc::downgrade(&self.inner);
        let target = self.inner.target.clone();
        tokio::spawn(async move {
            if target.resolve_all().await.is_none() {
                if let Some(inner) = inner.upgrade() {
                    BackendClient { inner }.trip();
                }
            }
        });
    }

    /// From now on, `report` hears each probe's outcome.
    pub fn report_probes_to(&self, report: ProbeReport) {
        *self.inner.report.lock().unwrap_or_else(PoisonError::into_inner) = Some(report);
    }

    /// The number of the client generation taking new requests, `1` for the
    /// first.
    pub fn generation(&self) -> u64 {
        self.inner.rotation.generation()
    }

    /// In-flight slots free now.
    pub fn free_slots(&self) -> usize {
        self.inner.slots.free()
    }

    /// The budgets of a retried request: `attempt` per attempt, inside
    /// `total_ms`, or inside the settings' ceiling when `total_ms <= 0`.
    pub fn budgets(&self, attempt: Duration, total_ms: i64) -> Budgets {
        let total = u64::try_from(total_ms)
            .ok()
            .filter(|ms| *ms > 0)
            .map_or(self.inner.ceiling, Duration::from_millis);
        Budgets { attempt, total }
    }

    /// One attempt of `req` under `budget` (capped by the ceiling), on the
    /// client taking new requests.
    pub async fn attempt(
        &self,
        req: HttpRequest,
        budget: Duration,
    ) -> Result<HttpResponse, AttemptError> {
        let client = self.inner.rotation.client();
        self.attempt_on(&client, req, budget).await
    }

    /// One attempt of `req` under `budget` on `client`.
    async fn attempt_on(
        &self,
        client: &Arc<dyn HttpClient>,
        req: HttpRequest,
        budget: Duration,
    ) -> Result<HttpResponse, AttemptError> {
        if self.is_open() {
            return Err(AttemptError::BreakerOpen);
        }
        let deadline = Instant::now() + budget.min(self.inner.ceiling);
        let Some(slot) = self.inner.slots.take(deadline).await else {
            return Err(AttemptError::Saturated);
        };
        if self.is_open() {
            return Err(AttemptError::BreakerOpen);
        }
        let mut sent = InFlight::sent(&self.inner);
        let outcome = tokio::time::timeout_at(deadline, client.request(req)).await;
        drop(slot);
        match outcome {
            Ok(Ok(resp)) => {
                sent.answered();
                Ok(resp)
            }
            Ok(Err(e)) => {
                sent.unanswered();
                Err(AttemptError::Transport(e))
            }
            Err(_) => {
                sent.unanswered();
                Err(AttemptError::Timeout)
            }
        }
    }
}

impl Inner {
    /// The breaker. Every step leaves it whole, so a poisoned lock is taken
    /// as it is.
    fn lock_breaker(&self) -> MutexGuard<'_, WindowBreaker> {
        self.breaker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `request` ended without a response: the rule may open the breaker.
    fn unanswered(self: &Arc<Self>, request: Sent) {
        let opened = self.lock_breaker().unanswered(request, Instant::now());
        if opened {
            self.opened();
        }
    }

    /// The breaker opened: forget the target's addresses, tell the
    /// watchers, start the probe.
    fn opened(self: &Arc<Self>) {
        self.target.forget();
        self.open.send_replace(true);
        metrics::breaker_moved(&self.peer, true);
        tracing::warn!(
            backend = %self.peer,
            window_ms = self.config.window.as_millis() as u64,
            min_sent = self.config.min_sent,
            "backend breaker open: requests fail at once until a probe of {} is answered",
            self.config.health_path
        );
        let mut probe = self.probe.lock().unwrap_or_else(PoisonError::into_inner);
        if probe.is_none() {
            let open = self.open.subscribe();
            *probe = Some(tokio::spawn(probe::run(Arc::downgrade(self), open)));
        }
    }

    /// A probe was answered: close the breaker.
    fn closed(&self) {
        let closed = self.lock_breaker().close(Instant::now());
        if closed {
            self.open.send_replace(false);
            metrics::breaker_moved(&self.peer, false);
            tracing::info!(backend = %self.peer, "backend breaker closed: a probe was answered");
        }
    }
}

/// A request counted by the breaker until it ends: a request whose caller
/// drops it mid-flight ends as [`WindowBreaker::dropped`].
struct InFlight<'a> {
    inner: &'a Arc<Inner>,
    request: Option<Sent>,
}

impl<'a> InFlight<'a> {
    /// A request leaving now.
    fn sent(inner: &'a Arc<Inner>) -> Self {
        let request = inner.lock_breaker().sent(Instant::now());
        Self { inner, request: Some(request) }
    }

    /// It got an HTTP response.
    fn answered(&mut self) {
        if let Some(request) = self.request.take() {
            self.inner.lock_breaker().answered(request, Instant::now());
        }
    }

    /// It ended without one.
    fn unanswered(&mut self) {
        if let Some(request) = self.request.take() {
            self.inner.unanswered(request);
        }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            self.inner.lock_breaker().dropped(request, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests;
