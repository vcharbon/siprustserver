//! [`LinkSettings`] — the replication link bounds a runner reads from env at
//! boot (ADR-0045), so each one is tuned by restarting a pod.
//!
//! ```text
//!   B2BUA_REPL_CONNECT_TIMEOUT_MS     one connect to a peer              (2000)
//!   B2BUA_REPL_IDLE_TIMEOUT_MS        a stream delivering nothing is left (15000)
//!   B2BUA_REPL_NOOP_INTERVAL_MS       the longest a served stream goes
//!                                     without a Noop                     (5000)
//!   B2BUA_REPL_SEND_STALL_MS          a send blocked this long leaves the
//!                                     stream, either end                 (15000)
//!   B2BUA_REPL_KEEPALIVE_IDLE_MS      TCP keepalive: idle before probing (5000)
//!   B2BUA_REPL_KEEPALIVE_INTERVAL_MS  TCP keepalive: between probes      (2000)
//!   B2BUA_REPL_KEEPALIVE_RETRIES      TCP keepalive: unanswered probes   (3)
//!   B2BUA_REPL_USER_TIMEOUT_MS        TCP_USER_TIMEOUT                   (15000)
//!   B2BUA_REPL_LOOKUP_TIMEOUT_MS      one peer name lookup               (2000)
//! ```
//!
//! Unset takes the default. A value that is not a positive integer refuses the
//! boot naming its variable, and so does one the kernel would not take as
//! given: keepalive times not whole seconds or above 32767 s, more than 127
//! keepalive probes, a user timeout above `u32::MAX` ms. So does an idle
//! timeout under twice the `Noop` interval: a healthy quiet stream could be
//! left between two `Noop`s. The two span nodes (one node's pullers, its
//! peers' servers): raise the idle timeout first, the interval after.

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
        // The kernel counts keepalive times in whole seconds, at most 32 767.
        let keepalive_s = |key: &str, default: Duration| {
            let v = ms(key, default)?;
            match v.as_millis() {
                n if n % 1000 == 0 && n <= 32_767_000 => Ok(v),
                _ => Err(format!("{key} must be a whole number of seconds, 1 s to 32767 s")),
            }
        };
        let settings = Self {
            connect_timeout: ms("B2BUA_REPL_CONNECT_TIMEOUT_MS", d.connect_timeout)?,
            idle_timeout: ms("B2BUA_REPL_IDLE_TIMEOUT_MS", d.idle_timeout)?,
            noop_interval: ms("B2BUA_REPL_NOOP_INTERVAL_MS", d.noop_interval)?,
            send_stall: ms("B2BUA_REPL_SEND_STALL_MS", d.send_stall)?,
            socket: SocketSettings {
                keepalive_idle: keepalive_s(
                    "B2BUA_REPL_KEEPALIVE_IDLE_MS",
                    d.socket.keepalive_idle,
                )?,
                keepalive_interval: keepalive_s(
                    "B2BUA_REPL_KEEPALIVE_INTERVAL_MS",
                    d.socket.keepalive_interval,
                )?,
                keepalive_retries: match positive(
                    &get,
                    "B2BUA_REPL_KEEPALIVE_RETRIES",
                    u64::from(d.socket.keepalive_retries),
                )? {
                    n @ 1..=127 => n as u32,
                    _ => return Err("B2BUA_REPL_KEEPALIVE_RETRIES must be 1 to 127".into()),
                },
                user_timeout: match ms("B2BUA_REPL_USER_TIMEOUT_MS", d.socket.user_timeout)? {
                    v if v.as_millis() <= u128::from(u32::MAX) => v,
                    _ => return Err("B2BUA_REPL_USER_TIMEOUT_MS is above 4294967295".into()),
                },
            },
            lookup_timeout: ms("B2BUA_REPL_LOOKUP_TIMEOUT_MS", d.lookup_timeout)?,
        };
        if settings.idle_timeout < settings.noop_interval * 2 {
            return Err(format!(
                "B2BUA_REPL_IDLE_TIMEOUT_MS ({} ms) must be at least twice \
                 B2BUA_REPL_NOOP_INTERVAL_MS ({} ms)",
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
            send_stall_ms: self.send_stall.as_millis() as u64,
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
