//! The probe of an open breaker: every [`BreakerConfig::probe_period`] it
//! forgets the target's addresses, looks the name up afresh (under the
//! target's lookup bound), then asks `GET` [`BreakerConfig::health_path`]
//! under [`BreakerConfig::probe_timeout`] on a client of its own that keeps
//! no pool, so each probe opens a new connection. Any HTTP response, of any status,
//! closes the breaker; a probe with none forgets the addresses again. A probe
//! that panics runs again the next period. Closed, the probe waits for the
//! next opening; it ends with the client.
//!
//! [`BreakerConfig::probe_period`]: super::BreakerConfig::probe_period
//! [`BreakerConfig::health_path`]: super::BreakerConfig::health_path
//! [`BreakerConfig::probe_timeout`]: super::BreakerConfig::probe_timeout

use std::sync::{PoisonError, Weak};

use tokio::sync::watch;

use crate::transport::{HttpError, HttpRequest};

use super::{AttemptError, Inner};

/// Probe while the breaker is open, until `inner` is gone; `open` is the
/// breaker's state. A probe that panics is run again the next period.
pub(super) async fn run(inner: Weak<Inner>, mut open: watch::Receiver<bool>) {
    loop {
        while !*open.borrow_and_update() {
            if open.changed().await.is_err() {
                return;
            }
        }
        match tokio::spawn(probe_until_closed(inner.clone())).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) if e.is_panic() => {
                tracing::warn!("backend breaker probe panicked: probing again next period");
            }
            Err(_) => return,
        }
    }
}

/// Probe every period until one is answered; `false` once `inner` is gone.
async fn probe_until_closed(inner: Weak<Inner>) -> bool {
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
                match tokio::time::timeout(inner.config.probe_timeout, client.request(health)).await
                {
                    Ok(Ok(resp)) => Ok(resp),
                    Ok(Err(e)) => Err(AttemptError::Transport(e)),
                    Err(_) => Err(AttemptError::Timeout),
                }
            }
            None => Err(AttemptError::Transport(HttpError::dns(&inner.peer))),
        };
        let report = inner.report.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(report) = report {
            report(answer.as_ref());
        }
        match answer {
            Ok(_) => {
                inner.closed();
                return true;
            }
            Err(_) => inner.target.forget(),
        }
    }
}
