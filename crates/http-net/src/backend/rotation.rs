//! [`Rotation`] — one backend's client generations. A new client
//! ([`HttpTransport::client`]) takes new requests once the rotation period
//! has passed since the current one was built and the target's fresh lookup,
//! started at the end of the period, is over, so a load balancer in front of
//! the backend sees new connections under steady traffic and a moved name is
//! reached by them; when the lookup finds nothing the last good addresses
//! serve. A request holds the client it started on: the old client
//! finishes its in-flight requests and closes its connections when the last
//! of them drops it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::poison::{locked, BACKEND_ROTATION};

use crate::resolved_target::ResolvedTarget;
use crate::settings::ClientSettings;
use crate::transport::{HttpClient, HttpTransport};

/// The client taking new requests, and when it was built.
struct Generation {
    client: Arc<dyn HttpClient>,
    since: Instant,
    number: u64,
    /// The period ended and the target's fresh lookup is running: the new
    /// client is built once it is over.
    refreshing: bool,
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
            current: Mutex::new(Generation {
                client,
                since: Instant::now(),
                number: 1,
                refreshing: false,
            }),
            transport,
            target,
            settings,
        }
    }

    /// The client taking new requests now. Once the period has passed, the
    /// target's fresh lookup starts behind the requests, which keep riding
    /// the current client; the new client takes over once that lookup is
    /// over, so its first connections go to what it found (or, when it found
    /// nothing, to the addresses kept).
    pub(super) fn client(&self) -> Arc<dyn HttpClient> {
        let mut current = locked(&self.current, BACKEND_ROTATION);
        let now = Instant::now();
        if now.duration_since(current.since) >= self.period {
            if !current.refreshing {
                self.target.refresh();
                self.target.look_up();
                current.refreshing = true;
            }
            if !self.target.is_refreshing() {
                current.client = self.transport.clone().client(self.target.clone(), &self.settings);
                current.since = now;
                current.number += 1;
                current.refreshing = false;
            }
        }
        current.client.clone()
    }

    /// The number of the client taking new requests, `1` for the first.
    pub(super) fn generation(&self) -> u64 {
        locked(&self.current, BACKEND_ROTATION).number
    }

    /// A client of its own keeping no pool, for one probe.
    pub(super) fn pool_less(&self) -> Arc<dyn HttpClient> {
        self.transport.clone().client(self.target.clone(), &self.settings.clone().without_pool())
    }
}
