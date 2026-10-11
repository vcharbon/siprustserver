//! The probe of an open breaker: every [`BreakerConfig::probe_period`] it
//! forgets the target's addresses, looks the name up afresh (under the
//! target's lookup bound), then asks `GET` [`BreakerConfig::health_path`]
//! under [`BreakerConfig::probe_timeout`] on a client of its own that keeps
//! no pool, so each probe opens a new connection. Any HTTP response, of any status,
//! closes the breaker; a probe with none forgets the addresses again. Closed,
//! the probe waits for the next opening; it ends with the client.
//!
//! [`BreakerConfig::probe_period`]: super::BreakerConfig::probe_period
//! [`BreakerConfig::health_path`]: super::BreakerConfig::health_path
//! [`BreakerConfig::probe_timeout`]: super::BreakerConfig::probe_timeout

use std::sync::{PoisonError, Weak};

use tokio::sync::watch;

use crate::transport::HttpRequest;

use super::Inner;

/// Probe while the breaker is open, until `inner` is gone; `open` is the
/// breaker's state.
pub(super) async fn run(inner: Weak<Inner>, mut open: watch::Receiver<bool>) {
    loop {
        while !*open.borrow_and_update() {
            if open.changed().await.is_err() {
                return;
            }
        }
        if !probe_until_closed(&inner).await {
            return;
        }
    }
}

/// Probe every period until one is answered; `false` once `inner` is gone.
async fn probe_until_closed(inner: &Weak<Inner>) -> bool {
    loop {
        let Some(period) = inner.upgrade().map(|i| i.config.probe_period) else {
            return false;
        };
        tokio::time::sleep(period).await;
        let Some(inner) = inner.upgrade() else {
            return false;
        };
        if !*inner.open.borrow() {
            return true;
        }
        inner.target.forget();
        let answer = match inner.target.resolve_all().await {
            Some(_) => {
                let client = inner.rotation.pool_less();
                let health = HttpRequest::get(inner.config.health_path.clone());
                tokio::time::timeout(inner.config.probe_timeout, client.request(health))
                    .await
                    .ok()
                    .and_then(Result::ok)
            }
            None => None,
        };
        let report = inner.report.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(report) = report {
            report(answer.as_ref());
        }
        match answer {
            Some(_) => {
                inner.closed();
                return true;
            }
            None => inner.target.forget(),
        }
    }
}
