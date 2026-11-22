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
//! | `<P>_CONN_CLOSE_STATUSES` | statuses whose first answer closes its connection ([`crate::ejection`]) | `500,502,504` |
//! | `<P>_CONN_SHED_STATUSES` | statuses whose answers in a row close their connection | `408,429,503` |
//! | `<P>_CONN_SHED_CLOSE_AFTER` | the run of shed answers on one connection that closes it | 3 |
//! | `<P>_ERROR_CLOSES_PER_S` | connections a client closes on its answers, at most, per second ([`crate::ErrorCloses`]); 0 never | 20 |
//! | `<P>_MAX_BODY_BYTES` | the largest answer body read: one declared or streamed past it is refused | [`crate::MAX_BODY_BYTES`] |
//!
//! Unset or blank takes the default. A duration or a count that is not a
//! positive integer, `<P>_ERROR_CLOSES_PER_S` that is not a non-negative
//! one, a status list with an entry that is not a status from 400 to 599,
//! two status lists sharing a status, or `<P>_MAX_BODY_BYTES` under
//! [`MIN_BODY_BYTES`] refuses the set, naming every variable involved.

use std::time::Duration;

use crate::ejection::EjectionRules;
use crate::env_reader::EnvReader;

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
    /// Which answers close the connection they came on, and how many such
    /// closes a client makes a second.
    pub ejection: EjectionRules,
    /// The largest answer body the client reads, at least
    /// [`MIN_BODY_BYTES`]: an answer declaring a longer one is refused before
    /// any of it is read, and one streaming past it is cut there.
    pub max_body_bytes: usize,
}

/// The cap on requests in flight a backend client runs with unless told
/// otherwise.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 256;

/// The smallest answer body cap the env may state.
pub const MIN_BODY_BYTES: usize = 1024;

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
            ejection: EjectionRules::default(),
            max_body_bytes: crate::MAX_BODY_BYTES,
        }
    }

    /// The same settings for a client that keeps no idle connection.
    pub fn without_pool(mut self) -> Self {
        self.max_idle_per_host = 0;
        self
    }

    /// The settings the env states under `prefix`, each unset value taken
    /// from `defaults`; an `Err` naming every variable that does not read
    /// (see the module doc).
    pub fn from_lookup(
        prefix: &str,
        defaults: &ClientSettings,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut env = EnvReader::new(prefix, lookup);
        let settings = Self::read(&mut env, defaults);
        env.finish(settings)
    }

    /// The settings `env` states, each unset or refused value taken from
    /// `defaults`, every refusal kept in `env`.
    pub(crate) fn read<L: Fn(&str) -> Option<String>>(
        env: &mut EnvReader<L>,
        defaults: &ClientSettings,
    ) -> Self {
        let max_in_flight = env.count("MAX_IN_FLIGHT", 1, defaults.max_in_flight);
        let d = &defaults.ejection;
        let ejection = EjectionRules {
            close_statuses: env.statuses("CONN_CLOSE_STATUSES", &d.close_statuses),
            shed_statuses: env.statuses("CONN_SHED_STATUSES", &d.shed_statuses),
            shed_close_after: env.count_u32("CONN_SHED_CLOSE_AFTER", 1, d.shed_close_after),
            closes_per_s: env.count_u32("ERROR_CLOSES_PER_S", 0, d.closes_per_s),
        };
        if !ejection.close_statuses.is_disjoint(&ejection.shed_statuses) {
            let (close, shed) = (env.key("CONN_CLOSE_STATUSES"), env.key("CONN_SHED_STATUSES"));
            env.refuse(format!(
                "{close}={} and {shed}={} share a status: one closes on its first answer, the \
                 other after a run",
                ejection.close_statuses, ejection.shed_statuses
            ));
        }
        Self {
            connect_timeout: env.ms("CONNECT_TIMEOUT_MS", defaults.connect_timeout),
            pool_idle_timeout: env.ms("POOL_IDLE_MS", defaults.pool_idle_timeout),
            keepalive_idle: env.ms("KEEPALIVE_IDLE_MS", defaults.keepalive_idle),
            keepalive_interval: env.ms("KEEPALIVE_INTERVAL_MS", defaults.keepalive_interval),
            keepalive_retries: env.count_u32("KEEPALIVE_RETRIES", 1, defaults.keepalive_retries),
            user_timeout: env.ms("USER_TIMEOUT_MS", defaults.user_timeout),
            ceiling: env.ms("CEILING_MS", defaults.ceiling),
            max_in_flight,
            max_idle_per_host: max_in_flight,
            rotation: env.ms("ROTATION_MS", defaults.rotation),
            ejection,
            max_body_bytes: env.count(
                "MAX_BODY_BYTES",
                MIN_BODY_BYTES as u64,
                defaults.max_body_bytes,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::ejection::StatusSet;

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
        let e = &settings.ejection;
        assert_eq!(
            (e.close_statuses.to_string(), e.shed_statuses.to_string()),
            ("500,502,504".to_string(), "408,429,503".to_string())
        );
        assert_eq!((e.shed_close_after, e.closes_per_s), (3, 20));
        assert_eq!(settings.max_body_bytes, 10 * 1024 * 1024);
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
            ("B_CONN_CLOSE_STATUSES", "500, 503"),
            ("B_CONN_SHED_STATUSES", "429"),
            ("B_CONN_SHED_CLOSE_AFTER", "1"),
            ("B_ERROR_CLOSES_PER_S", "0"),
            ("B_MAX_BODY_BYTES", "1024"),
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
                ejection: EjectionRules {
                    close_statuses: StatusSet::of(&[500, 503]).unwrap(),
                    shed_statuses: StatusSet::of(&[429]).unwrap(),
                    shed_close_after: 1,
                    closes_per_s: 0,
                },
                max_body_bytes: 1024,
            }
        );
    }

    #[test]
    fn a_value_that_is_not_a_positive_integer_is_named() {
        let e = from(&[("B_MAX_IN_FLIGHT", "0")]).expect_err("zero");
        assert!(e.contains("B_MAX_IN_FLIGHT=0"), "{e}");
        let e = from(&[("B_ROTATION_MS", "soon")]).expect_err("not a number");
        assert!(e.contains("B_ROTATION_MS"), "{e}");
        let e = from(&[("B_ERROR_CLOSES_PER_S", "-1")]).expect_err("negative");
        assert!(e.contains("B_ERROR_CLOSES_PER_S"), "{e}");
        assert_eq!(from(&[("B_CEILING_MS", " ")]).unwrap().ceiling, Duration::from_secs(5));
    }

    #[test]
    fn an_invalid_ejection_set_is_refused_naming_every_variable_involved() {
        let e = from(&[
            ("B_CONN_CLOSE_STATUSES", "500,503"),
            ("B_CONN_SHED_CLOSE_AFTER", "0"),
            ("B_MAX_BODY_BYTES", "1023"),
            ("B_CEILING_MS", "soon"),
        ])
        .expect_err("four refusals");
        let refusals: Vec<&str> = e.split("; ").collect();
        assert_eq!(refusals.len(), 4, "{e}");
        for named in [
            "B_CONN_SHED_CLOSE_AFTER=0: must be positive",
            "B_CONN_CLOSE_STATUSES=500,503 and B_CONN_SHED_STATUSES=408,429,503 share a status",
            "B_CEILING_MS=\"soon\"",
            "B_MAX_BODY_BYTES=1023: must be at least 1024",
        ] {
            assert!(refusals.iter().any(|r| r.starts_with(named)), "{named} in {e}");
        }
        let e = from(&[("B_CONN_SHED_STATUSES", "429,200")]).expect_err("not a 4xx or 5xx");
        assert!(e.starts_with("B_CONN_SHED_STATUSES=\"429,200\": \"200\""), "{e}");
    }

    #[test]
    fn a_client_without_pool_keeps_no_idle_connection() {
        let settings = ClientSettings::with_max_in_flight(8).without_pool();
        assert_eq!((settings.max_in_flight, settings.max_idle_per_host), (8, 0));
    }
}
