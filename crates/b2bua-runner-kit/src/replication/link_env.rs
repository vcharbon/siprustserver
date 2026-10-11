//! [`LinkSettings`] — the replication link bounds a runner reads from env at
//! boot (ADR-0045), so each one is tuned by restarting a pod.
//!
//! ```text
//!   B2BUA_REPL_CONNECT_TIMEOUT_MS     one connect to a peer              (2000)
//!   B2BUA_REPL_IDLE_TIMEOUT_MS        a stream delivering nothing is left (15000)
//!   B2BUA_REPL_NOOP_INTERVAL_MS       the longest a served stream goes
//!                                     without a Noop                     (5000)
//!   B2BUA_REPL_SEND_STALL_MS          a send blocked this long drops the
//!                                     stream                             (15000)
//!   B2BUA_REPL_KEEPALIVE_IDLE_MS      TCP keepalive: idle before probing (5000)
//!   B2BUA_REPL_KEEPALIVE_INTERVAL_MS  TCP keepalive: between probes      (2000)
//!   B2BUA_REPL_KEEPALIVE_RETRIES      TCP keepalive: unanswered probes   (3)
//!   B2BUA_REPL_USER_TIMEOUT_MS        TCP_USER_TIMEOUT                   (15000)
//!   B2BUA_REPL_LOOKUP_TIMEOUT_MS      one peer name lookup               (2000)
//! ```
//!
//! Unset takes the default. A value that is not a positive integer refuses the
//! boot naming its variable, and so does an idle timeout not above the `Noop`
//! interval: every healthy quiet stream would be left.

use std::time::Duration;

use b2bua::repl::{PullerConfig, ServeConfig};
use repl_net::SocketSettings;

/// The replication link bounds, read once at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkSettings {
    pub(crate) connect_timeout: Duration,
    pub(crate) idle_timeout: Duration,
    pub(crate) noop_interval: Duration,
    pub(crate) send_stall: Duration,
    pub(crate) socket: SocketSettings,
    pub(crate) lookup_timeout: Duration,
}

impl Default for LinkSettings {
    fn default() -> Self {
        let puller = PullerConfig::default();
        let serve = ServeConfig::default();
        Self {
            connect_timeout: Duration::from_millis(puller.connect_timeout_ms),
            idle_timeout: Duration::from_millis(puller.idle_timeout_ms),
            noop_interval: serve.noop_interval,
            send_stall: serve.send_stall,
            socket: SocketSettings::default(),
            lookup_timeout: http_net::LOOKUP_BOUND,
        }
    }
}

impl LinkSettings {
    /// Reads the link variables through `get`; `Err` names the variable.
    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = Self::default();
        let ms = |key: &str, default: Duration| {
            positive(&get, key, default.as_millis() as u64).map(Duration::from_millis)
        };
        let settings = Self {
            connect_timeout: ms("B2BUA_REPL_CONNECT_TIMEOUT_MS", d.connect_timeout)?,
            idle_timeout: ms("B2BUA_REPL_IDLE_TIMEOUT_MS", d.idle_timeout)?,
            noop_interval: ms("B2BUA_REPL_NOOP_INTERVAL_MS", d.noop_interval)?,
            send_stall: ms("B2BUA_REPL_SEND_STALL_MS", d.send_stall)?,
            socket: SocketSettings {
                keepalive_idle: ms("B2BUA_REPL_KEEPALIVE_IDLE_MS", d.socket.keepalive_idle)?,
                keepalive_interval: ms(
                    "B2BUA_REPL_KEEPALIVE_INTERVAL_MS",
                    d.socket.keepalive_interval,
                )?,
                keepalive_retries: u32::try_from(positive(
                    &get,
                    "B2BUA_REPL_KEEPALIVE_RETRIES",
                    u64::from(d.socket.keepalive_retries),
                )?)
                .map_err(|_| "B2BUA_REPL_KEEPALIVE_RETRIES is out of range".to_string())?,
                user_timeout: ms("B2BUA_REPL_USER_TIMEOUT_MS", d.socket.user_timeout)?,
            },
            lookup_timeout: ms("B2BUA_REPL_LOOKUP_TIMEOUT_MS", d.lookup_timeout)?,
        };
        if settings.idle_timeout <= settings.noop_interval {
            return Err(format!(
                "B2BUA_REPL_IDLE_TIMEOUT_MS ({} ms) must exceed B2BUA_REPL_NOOP_INTERVAL_MS \
                 ({} ms)",
                settings.idle_timeout.as_millis(),
                settings.noop_interval.as_millis()
            ));
        }
        Ok(settings)
    }

    /// The pullers' configuration: the default backoff and bootstrap timer,
    /// these link bounds.
    pub(crate) fn puller(&self) -> PullerConfig {
        PullerConfig {
            connect_timeout_ms: self.connect_timeout.as_millis() as u64,
            idle_timeout_ms: self.idle_timeout.as_millis() as u64,
            ..PullerConfig::default()
        }
    }

    /// The serve loop's configuration.
    pub(crate) fn serve(&self) -> ServeConfig {
        ServeConfig { noop_interval: self.noop_interval, send_stall: self.send_stall }
    }
}

/// `key` as a positive integer, `default` when unset.
fn positive(get: &impl Fn(&str) -> Option<String>, key: &str, default: u64) -> Result<u64, String> {
    match get(key) {
        None => Ok(default),
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(v) if v > 0 => Ok(v),
            _ => Err(format!("{key} must be a positive integer, got {raw:?}")),
        },
    }
}

#[cfg(test)]
mod tests;
