//! [`PooledClient`] — the fabric's backend client: keep-alive connections
//! held as the real pooled client holds them.
//!
//! - A request takes the idle connection returned most recently, else opens
//!   a new one (to a backend its [`Front`](super::front) chooses when the
//!   target's address is balanced).
//! - A connection that carried an answer goes back to the pool, at most
//!   `max_idle_per_host` of them, unless the client's ejection rules close
//!   it ([`Ejector`]: the first answer of a close status, a run of shed
//!   answers, within the client's budget); the answer is returned whole
//!   either way. One whose exchange failed is closed. One whose request its
//!   caller dropped mid-flight is closed and counted as a timeout close.
//! - An answer whose body passes `max_body_bytes` fails `BodyTooLarge`,
//!   counted as a declared refusal, after the ejection rules judged its
//!   status; its connection is closed.
//! - An idle connection is closed past `pool_idle_timeout`, or when the pool
//!   holds `max_idle_per_host` already (counted as idle closes), and when its
//!   backend closed it (see [`SimulatedHttpNetwork`]); a closed one is never
//!   handed out.
//! - Dropping the client closes its idle connections (counted as rotation
//!   closes).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use http_net::backend::metrics::{self, BodyRefusal};
use http_net::ejection::{self, CloseCause, Ejector};
use http_net::failures::FailureCause;
use http_net::{HttpClient, ResolvedTarget};
use tokio::time::Instant;

use super::{Connection, SimulatedHttpNetwork};
use crate::settings::ClientSettings;
use crate::{HttpError, HttpRequest, HttpResponse};

/// One backend's pooled client over the fabric.
pub(super) struct PooledClient {
    net: SimulatedHttpNetwork,
    target: Arc<ResolvedTarget>,
    idle_timeout: Duration,
    max_idle: usize,
    /// Idle connections and when each came back, the most recent last.
    idle: Mutex<Vec<(Connection, Instant)>>,
    ejector: Ejector,
    /// The largest answer body returned.
    max_body: usize,
    /// The target as configured: the close counters' `peer`.
    peer: String,
}

impl PooledClient {
    pub(super) fn new(
        net: SimulatedHttpNetwork,
        target: Arc<ResolvedTarget>,
        settings: &ClientSettings,
    ) -> Self {
        Self {
            net,
            target: target.clone(),
            idle_timeout: settings.pool_idle_timeout,
            max_idle: settings.max_idle_per_host,
            idle: Mutex::new(Vec::new()),
            ejector: Ejector::new(&settings.ejection),
            max_body: settings.max_body_bytes,
            peer: target.to_string(),
        }
    }

    /// The idle connection to `addr` returned most recently and still open,
    /// the expired and closed ones closed on the way.
    fn checkout(&self, addr: SocketAddr) -> Option<Connection> {
        let now = Instant::now();
        let mut idle = self.idle.lock().unwrap();
        idle.retain(|(conn, since)| {
            if !self.net.is_open(conn) {
                return false;
            }
            let fresh = now.duration_since(*since) < self.idle_timeout;
            if !fresh {
                ejection::closed(&self.peer, CloseCause::Idle);
            }
            fresh
        });
        let at = idle.iter().rposition(|(conn, _)| conn.front() == addr)?;
        Some(idle.remove(at).0)
    }

    fn give_back(&self, conn: Connection) {
        if !self.net.is_open(&conn) {
            return;
        }
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < self.max_idle {
            idle.push((conn, Instant::now()));
        } else if self.max_idle > 0 {
            ejection::closed(&self.peer, CloseCause::Idle);
        }
    }
}

impl Drop for PooledClient {
    fn drop(&mut self) {
        let idle = self.idle.get_mut().unwrap_or_else(PoisonError::into_inner);
        for (conn, _) in idle.iter() {
            if self.net.is_open(conn) {
                ejection::closed(&self.peer, CloseCause::Rotation);
            }
        }
    }
}

/// A request in flight on a connection: dropped before its exchange ended,
/// it abandons the connection.
struct InFlight<'a> {
    client: &'a PooledClient,
    ended: bool,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.client.ejector.abandoned(&self.client.peer);
        }
    }
}

#[async_trait]
impl HttpClient for PooledClient {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let Some(addr) = self.target.resolve().await else {
            return Err(HttpError::dns(&*self.target));
        };
        let mut conn = match self.checkout(addr) {
            Some(conn) => conn,
            None => self.net.connect(addr),
        };
        let mut in_flight = InFlight { client: self, ended: false };
        let outcome = self.net.exchange(&mut conn, req).await;
        in_flight.ended = true;
        let resp = outcome?;
        let verdict = self.ejector.answered(&conn.record, resp.status);
        let closes = verdict.closes().is_some();
        verdict.count(&self.peer);
        if resp.body.len() > self.max_body {
            metrics::body_refused(&self.peer, BodyRefusal::Declared);
            let why = format!("response body past {} bytes", self.max_body);
            return Err(HttpError::new(&self.peer, FailureCause::BodyTooLarge, why));
        }
        if !closes {
            self.give_back(conn);
        }
        Ok(resp)
    }
}
