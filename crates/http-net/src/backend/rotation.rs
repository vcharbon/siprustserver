//! [`Rotation`] — one backend's client generations. A new client
//! ([`HttpTransport::client`]) takes new requests once the rotation period
//! has passed since the current one was built, so a load balancer in front
//! of the backend sees new connections under steady traffic; the target is
//! refreshed at the same time, so the new client looks the name up afresh,
//! serving the last good addresses when that lookup finds nothing. A request holds the client it started on: the old client
//! finishes its in-flight requests and closes its connections when the last
//! of them drops it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::resolved_target::ResolvedTarget;
use crate::settings::ClientSettings;
use crate::transport::{HttpClient, HttpTransport};

/// The client taking new requests, and when it was built.
struct Generation {
    client: Arc<dyn HttpClient>,
    since: Instant,
    number: u64,
}

/// One backend's client generations.
pub(super) struct Rotation {
    transport: Arc<dyn HttpTransport>,
    target: Arc<ResolvedTarget>,
    settings: ClientSettings,
    period: Duration,
    current: Mutex<Generation>,
}

impl Rotation {
    /// Generations of `target`'s client over `transport`, the first built now.
    pub(super) fn new(
        transport: Arc<dyn HttpTransport>,
        target: Arc<ResolvedTarget>,
        settings: ClientSettings,
    ) -> Self {
        let client = transport.clone().client(target.clone(), &settings);
        Self {
            period: settings.rotation,
            current: Mutex::new(Generation { client, since: Instant::now(), number: 1 }),
            transport,
            target,
            settings,
        }
    }

    /// The client taking new requests now, a new one when the period has
    /// passed.
    pub(super) fn client(&self) -> Arc<dyn HttpClient> {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if now.duration_since(current.since) >= self.period {
            self.target.refresh();
            current.client = self.transport.clone().client(self.target.clone(), &self.settings);
            current.since = now;
            current.number += 1;
        }
        current.client.clone()
    }

    /// The number of the client taking new requests, `1` for the first.
    pub(super) fn generation(&self) -> u64 {
        self.current.lock().unwrap_or_else(PoisonError::into_inner).number
    }

    /// A client of its own keeping no pool, for one probe.
    pub(super) fn pool_less(&self) -> Arc<dyn HttpClient> {
        self.transport.clone().client(self.target.clone(), &self.settings.clone().without_pool())
    }
}
