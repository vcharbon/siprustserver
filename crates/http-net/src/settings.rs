//! [`ClientSettings`] — how one backend's client holds its connections and
//! bounds its requests (ADR-0043), and the env grammar that states them.
//!
//! | variable (`<P>` the backend's prefix) | meaning | default |
//! |---|---|---|
//! | `<P>_CONNECT_TIMEOUT_MS` | the bound on one TCP connect | 1000 |
//! | `<P>_POOL_IDLE_MS` | an idle pooled connection is closed past it | 4000 |
//! | `<P>_KEEPALIVE_IDLE_MS` | TCP keepalive: idle time before the first probe | 5000 |
//! | `<P>_KEEPALIVE_INTERVAL_MS` | TCP keepalive: time between probes | 2000 |
//! | `<P>_KEEPALIVE_RETRIES` | TCP keepalive: unanswered probes that drop the connection | 3 |
//! | `<P>_USER_TIMEOUT_MS` | `TCP_USER_TIMEOUT`: unacknowledged data drops the connection past it | 10000 |
//! | `<P>_CEILING_MS` | the bound on any one request, whatever its caller's budget | 5000 |
//! | `<P>_MAX_IN_FLIGHT` | requests in flight at once, and idle connections kept | per backend |
//! | `<P>_ROTATION_MS` | a new client takes new requests this often | 60000 |
//!
//! Unset or blank takes the default; a value that is not a positive integer
//! is an error naming it.

use std::time::Duration;

/// One backend client's connection and request bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientSettings {
    /// The bound on one TCP connect.
    pub connect_timeout: Duration,
    /// An idle pooled connection is closed past it: below a peer's own
    /// keep-alive timeout, so the client never reuses a connection the peer
    /// is closing.
    pub pool_idle_timeout: Duration,
    /// TCP keepalive: idle time before the first probe.
    pub keepalive_idle: Duration,
    /// TCP keepalive: time between probes.
    pub keepalive_interval: Duration,
    /// TCP keepalive: unanswered probes that drop the connection.
    pub keepalive_retries: u32,
    /// `TCP_USER_TIMEOUT`: data unacknowledged this long drops the connection.
    pub user_timeout: Duration,
    /// The bound on any one request, whatever its caller's budget: no
    /// request is ever unbounded.
    pub ceiling: Duration,
    /// Requests in flight at once; the cap on TCP connections.
    pub max_in_flight: usize,
    /// Idle connections kept per host: `max_in_flight`, `0` for a client
    /// that keeps no pool.
    pub max_idle_per_host: usize,
    /// A new client takes new requests this often; the old one finishes the
    /// requests it carries and closes its connections.
    pub rotation: Duration,
}

/// The cap on requests in flight a backend client runs with unless told
/// otherwise.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 256;

impl Default for ClientSettings {
    fn default() -> Self {
        Self::with_max_in_flight(DEFAULT_MAX_IN_FLIGHT)
    }
}

impl ClientSettings {
    /// The default settings with `max_in_flight` requests in flight at once.
    pub fn with_max_in_flight(max_in_flight: usize) -> Self {
        let max_in_flight = max_in_flight.max(1);
        Self {
            connect_timeout: Duration::from_millis(1_000),
            pool_idle_timeout: Duration::from_millis(4_000),
            keepalive_idle: Duration::from_millis(5_000),
            keepalive_interval: Duration::from_millis(2_000),
            keepalive_retries: 3,
            user_timeout: Duration::from_millis(10_000),
            ceiling: Duration::from_millis(5_000),
            max_in_flight,
            max_idle_per_host: max_in_flight,
            rotation: Duration::from_millis(60_000),
        }
    }

    /// The same settings for a client that keeps no idle connection.
    pub fn without_pool(mut self) -> Self {
        self.max_idle_per_host = 0;
        self
    }

    /// The settings the env states under `prefix`, each unset value taken
    /// from `defaults`. A value that is not a positive integer is an `Err`
    /// naming it.
    pub fn from_lookup(
        prefix: &str,
        defaults: &ClientSettings,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let positive = |suffix: &str, default: u64| -> Result<u64, String> {
            let key = format!("{prefix}_{suffix}");
            match lookup(&key).filter(|v| !v.trim().is_empty()) {
                None => Ok(default),
                Some(v) => match v.trim().parse::<u64>() {
                    Ok(0) => Err(format!("{key}=0: must be positive")),
                    Ok(n) => Ok(n),
                    Err(e) => Err(format!("{key}={v:?}: {e}")),
                },
            }
        };
        let ms = |suffix: &str, default: Duration| -> Result<Duration, String> {
            let default = u64::try_from(default.as_millis()).unwrap_or(u64::MAX);
            positive(suffix, default).map(Duration::from_millis)
        };
        let max_in_flight = positive("MAX_IN_FLIGHT", defaults.max_in_flight as u64)?;
        let max_in_flight = usize::try_from(max_in_flight)
            .map_err(|_| format!("{prefix}_MAX_IN_FLIGHT={max_in_flight}: past this platform"))?;
        let retries = positive("KEEPALIVE_RETRIES", u64::from(defaults.keepalive_retries))?;
        Ok(Self {
            connect_timeout: ms("CONNECT_TIMEOUT_MS", defaults.connect_timeout)?,
            pool_idle_timeout: ms("POOL_IDLE_MS", defaults.pool_idle_timeout)?,
            keepalive_idle: ms("KEEPALIVE_IDLE_MS", defaults.keepalive_idle)?,
            keepalive_interval: ms("KEEPALIVE_INTERVAL_MS", defaults.keepalive_interval)?,
            keepalive_retries: u32::try_from(retries)
                .map_err(|_| format!("{prefix}_KEEPALIVE_RETRIES={retries}: too large"))?,
            user_timeout: ms("USER_TIMEOUT_MS", defaults.user_timeout)?,
            ceiling: ms("CEILING_MS", defaults.ceiling)?,
            max_in_flight,
            max_idle_per_host: max_in_flight,
            rotation: ms("ROTATION_MS", defaults.rotation)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn from(pairs: &[(&str, &str)]) -> Result<ClientSettings, String> {
        let env: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ClientSettings::from_lookup("B", &ClientSettings::with_max_in_flight(64), |k| {
            env.get(k).cloned()
        })
    }

    #[test]
    fn unset_takes_the_defaults() {
        let settings = from(&[]).unwrap();
        assert_eq!(settings, ClientSettings::with_max_in_flight(64));
        assert_eq!(settings.connect_timeout, Duration::from_secs(1));
        assert_eq!(settings.pool_idle_timeout, Duration::from_secs(4));
        assert_eq!(
            (settings.keepalive_idle, settings.keepalive_interval, settings.keepalive_retries),
            (Duration::from_secs(5), Duration::from_secs(2), 3)
        );
        assert_eq!(settings.user_timeout, Duration::from_secs(10));
        assert_eq!(settings.ceiling, Duration::from_secs(5));
        assert_eq!(settings.rotation, Duration::from_secs(60));
        assert_eq!((settings.max_in_flight, settings.max_idle_per_host), (64, 64));
    }

    #[test]
    fn every_value_is_set_through_the_env() {
        let settings = from(&[
            ("B_CONNECT_TIMEOUT_MS", "200"),
            ("B_POOL_IDLE_MS", "3000"),
            ("B_KEEPALIVE_IDLE_MS", "6000"),
            ("B_KEEPALIVE_INTERVAL_MS", "1000"),
            ("B_KEEPALIVE_RETRIES", "5"),
            ("B_USER_TIMEOUT_MS", "8000"),
            ("B_CEILING_MS", "4000"),
            ("B_MAX_IN_FLIGHT", "16"),
            ("B_ROTATION_MS", "30000"),
        ])
        .unwrap();
        assert_eq!(
            settings,
            ClientSettings {
                connect_timeout: Duration::from_millis(200),
                pool_idle_timeout: Duration::from_millis(3_000),
                keepalive_idle: Duration::from_millis(6_000),
                keepalive_interval: Duration::from_millis(1_000),
                keepalive_retries: 5,
                user_timeout: Duration::from_millis(8_000),
                ceiling: Duration::from_millis(4_000),
                max_in_flight: 16,
                max_idle_per_host: 16,
                rotation: Duration::from_millis(30_000),
            }
        );
    }

    #[test]
    fn a_value_that_is_not_a_positive_integer_is_named() {
        let e = from(&[("B_MAX_IN_FLIGHT", "0")]).expect_err("zero");
        assert!(e.contains("B_MAX_IN_FLIGHT=0"), "{e}");
        let e = from(&[("B_ROTATION_MS", "soon")]).expect_err("not a number");
        assert!(e.contains("B_ROTATION_MS"), "{e}");
        assert_eq!(from(&[("B_CEILING_MS", " ")]).unwrap().ceiling, Duration::from_secs(5));
    }

    #[test]
    fn a_client_without_pool_keeps_no_idle_connection() {
        let settings = ClientSettings::with_max_in_flight(8).without_pool();
        assert_eq!((settings.max_in_flight, settings.max_idle_per_host), (8, 0));
    }
}
