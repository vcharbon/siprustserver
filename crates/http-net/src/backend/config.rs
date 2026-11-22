//! [`BackendConfig`] — everything one backend client is tuned by, and the env
//! grammar that states it under the backend's prefix, read at boot
//! (ADR-0043). The transport keys are [`ClientSettings`]'s
//! ([`crate::settings`]); the others:
//!
//! | variable (`<P>` the backend's prefix) | meaning | default |
//! |---|---|---|
//! | `<P>_TIMEOUT_MS` | one attempt's budget, its slot wait included | per backend |
//! | `<P>_BREAKER_REQUESTS` | requests in flight during the window, none answered, that open the breaker | 3 |
//! | `<P>_BREAKER_WINDOW_MS` | the window the breaker looks back over | 2000 |
//! | `<P>_BREAKER_PROBE_MS` | the probe period while open | 1000 |
//! | `<P>_BREAKER_PROBE_TIMEOUT_MS` | one probe request's budget, after its lookup | the attempt budget, at most the probe period |
//! | `<P>_HEALTH_PATH` | the path the probe asks with `GET` | per backend |
//! | `<P>_LOOKUP_TIMEOUT_MS` | the bound on one name lookup | 2000 |
//! | `<P>_LOOKUP_MAX_STUCK` | lookups left running past their bound before none starts | 4 |
//!
//! Unset or blank takes the backend's default; a duration or a count that is
//! not a positive integer, or a health path not starting with `/`, refuses
//! the set with the transport keys' refusals, one error naming every
//! variable that does not read.

use std::time::Duration;

use super::BreakerConfig;
use crate::env_reader::EnvReader;
use crate::resolved_target::LookupSettings;
use crate::settings::ClientSettings;

/// One backend client's tuning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendConfig {
    /// Its connections and bounds.
    pub client: ClientSettings,
    /// Its breaker.
    pub breaker: BreakerConfig,
    /// Its name lookups.
    pub lookup: LookupSettings,
    /// One attempt's budget.
    pub attempt: Duration,
}

impl BackendConfig {
    /// The defaults of a backend capped at `max_in_flight` requests in
    /// flight, each attempt under `attempt`, its probe asking `health_path`
    /// under `attempt`, at most the probe period, so a probe runs about once
    /// a period.
    pub fn new(max_in_flight: usize, attempt: Duration, health_path: &str) -> Self {
        let breaker = BreakerConfig::probing(health_path);
        Self {
            client: ClientSettings::with_max_in_flight(max_in_flight),
            breaker: BreakerConfig { probe_timeout: attempt.min(breaker.probe_period), ..breaker },
            lookup: LookupSettings::default(),
            attempt,
        }
    }

    /// The config the env states under `prefix`, each unset value taken from
    /// `defaults`; an `Err` naming every variable that does not read. See the
    /// module doc.
    pub fn from_lookup(
        prefix: &str,
        defaults: &BackendConfig,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut env = EnvReader::new(prefix, lookup);
        let health_path = match env.stated("HEALTH_PATH") {
            None => defaults.breaker.health_path.clone(),
            Some(path) if path.starts_with('/') => path,
            Some(path) => {
                let key = env.key("HEALTH_PATH");
                env.refuse(format!("{key}={path:?}: a path starts with /"));
                defaults.breaker.health_path.clone()
            }
        };
        let d = &defaults.breaker;
        let config = Self {
            client: ClientSettings::read(&mut env, &defaults.client),
            breaker: BreakerConfig {
                window: env.ms("BREAKER_WINDOW_MS", d.window),
                min_sent: env.count("BREAKER_REQUESTS", 1, d.min_sent),
                probe_period: env.ms("BREAKER_PROBE_MS", d.probe_period),
                probe_timeout: env.ms("BREAKER_PROBE_TIMEOUT_MS", d.probe_timeout),
                health_path,
            },
            lookup: LookupSettings {
                bound: env.ms("LOOKUP_TIMEOUT_MS", defaults.lookup.bound),
                max_stuck: env.count("LOOKUP_MAX_STUCK", 1, defaults.lookup.max_stuck),
            },
            attempt: env.ms("TIMEOUT_MS", defaults.attempt),
        };
        env.finish(config)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn defaults() -> BackendConfig {
        BackendConfig::new(256, Duration::from_millis(2_500), "/")
    }

    fn from(pairs: &[(&str, &str)]) -> Result<BackendConfig, String> {
        let env: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        BackendConfig::from_lookup("DECIDE", &defaults(), |k| env.get(k).cloned())
    }

    #[test]
    fn unset_takes_the_defaults() {
        let config = from(&[]).unwrap();
        assert_eq!(config, defaults());
        assert_eq!(config.breaker.min_sent, 3);
        assert_eq!(config.breaker.window, Duration::from_secs(2));
        assert_eq!(config.breaker.probe_period, Duration::from_secs(1));
        assert_eq!(config.breaker.probe_timeout, Duration::from_secs(1), "at most the period");
        assert_eq!(config.lookup, LookupSettings { bound: Duration::from_secs(2), max_stuck: 4 });
        assert_eq!(config.attempt, Duration::from_millis(2_500));
    }

    #[test]
    fn every_value_is_set_through_the_env() {
        let config = from(&[
            ("DECIDE_TIMEOUT_MS", "1000"),
            ("DECIDE_BREAKER_REQUESTS", "50"),
            ("DECIDE_BREAKER_WINDOW_MS", "3000"),
            ("DECIDE_BREAKER_PROBE_MS", "500"),
            ("DECIDE_BREAKER_PROBE_TIMEOUT_MS", "400"),
            ("DECIDE_HEALTH_PATH", "/ready"),
            ("DECIDE_LOOKUP_TIMEOUT_MS", "1500"),
            ("DECIDE_LOOKUP_MAX_STUCK", "8"),
            ("DECIDE_MAX_IN_FLIGHT", "32"),
        ])
        .unwrap();
        assert_eq!(config.attempt, Duration::from_secs(1));
        assert_eq!(
            config.breaker,
            BreakerConfig {
                window: Duration::from_secs(3),
                min_sent: 50,
                probe_period: Duration::from_millis(500),
                probe_timeout: Duration::from_millis(400),
                health_path: "/ready".into(),
            }
        );
        assert_eq!(
            config.lookup,
            LookupSettings { bound: Duration::from_millis(1_500), max_stuck: 8 }
        );
        assert_eq!(config.client.max_in_flight, 32);
    }

    #[test]
    fn a_bad_value_is_named() {
        for (key, value) in [
            ("DECIDE_BREAKER_REQUESTS", "0"),
            ("DECIDE_LOOKUP_TIMEOUT_MS", "2s"),
            ("DECIDE_HEALTH_PATH", "health"),
            ("DECIDE_CONNECT_TIMEOUT_MS", "0"),
        ] {
            let e = from(&[(key, value)]).expect_err(key);
            assert!(e.contains(key), "{e}");
        }
    }

    #[test]
    fn one_refusal_names_every_bad_value_of_both_grammars() {
        let e = from(&[
            ("DECIDE_HEALTH_PATH", "health"),
            ("DECIDE_MAX_BODY_BYTES", "12"),
            ("DECIDE_LOOKUP_MAX_STUCK", "0"),
        ])
        .expect_err("three bad values");
        for key in ["DECIDE_HEALTH_PATH", "DECIDE_MAX_BODY_BYTES", "DECIDE_LOOKUP_MAX_STUCK"] {
            assert!(e.contains(key), "{key} in {e}");
        }
        assert_eq!(e.matches("; ").count(), 2, "{e}");
    }
}
