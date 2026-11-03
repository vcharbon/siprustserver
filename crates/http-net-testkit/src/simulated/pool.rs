//! [`PooledClient`] — the fabric's backend client: keep-alive connections
//! held as a pooled HTTP/1.1 client holds them.
//!
//! - A request takes the idle connection returned most recently, else opens
//!   a new one (to a backend its [`Front`](super::front) chooses when the
//!   target's address is balanced).
//! - A connection that carried an answer goes back to the pool, at most
//!   `max_idle_per_host` of them; one that failed, or whose request its
//!   caller dropped mid-flight, is closed. One that carried a 5xx is closed
//!   within the client's [`ErrorCloses`] budget (`error_closes_per_s`), the
//!   answer returned with its body dropped.
//! - An idle connection is closed past `pool_idle_timeout`, and when its
//!   backend closed it (see [`SimulatedHttpNetwork`]); a closed one is never
//!   handed out.
//! - Dropping the client closes its idle connections.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use http_net::{ErrorCloses, HttpClient, ResolvedTarget};
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
    closes: ErrorCloses,
    /// The target as configured: the close counter's `peer`.
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
            closes: ErrorCloses::new(settings.error_closes_per_s),
            peer: target.to_string(),
        }
    }

    /// The idle connection to `addr` returned most recently and still open,
    /// the expired and closed ones closed on the way.
    fn checkout(&self, addr: SocketAddr) -> Option<Connection> {
        let now = Instant::now();
        let mut idle = self.idle.lock().unwrap();
        idle.retain(|(conn, since)| {
            now.duration_since(*since) < self.idle_timeout && self.net.is_open(conn)
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
        let mut outcome = self.net.exchange(&mut conn, req).await;
        match &mut outcome {
            Ok(resp) if self.closes.close_after(&self.peer, resp.status) => resp.body.clear(),
            Ok(_) => self.give_back(conn),
            Err(_) => {}
        }
        outcome
    }
}
