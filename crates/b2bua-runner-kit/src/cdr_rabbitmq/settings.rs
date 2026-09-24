//! The RabbitMQ CDR sink's env grammar, `B2BUA_CDR_RABBITMQ_*`: the broker,
//! the destination queue and how it is declared, and the bounds of every
//! wait the sink makes on the broker.

use std::time::Duration;

/// How the writer declares its destination queue, `B2BUA_CDR_RABBITMQ_DECLARE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdrQueueDeclare {
    /// `own` (default): the writer declares the queue durable, bounded by
    /// `x-max-length` with `x-overflow=drop-head` (`B2BUA_CDR_RABBITMQ_MAX_LEN`,
    /// default 100000; `0` disables the bound). Every other declarer of the
    /// queue must pass the same arguments, or the broker refuses the later,
    /// mismatching declare (406 PRECONDITION_FAILED).
    Own { max_len: i64 },
    /// `existing`: the broker already holds the queue; the writer declares it
    /// passively (fails if absent) and never states its arguments, so a queue
    /// with arguments of its own (a quorum queue, a dead-letter exchange) is
    /// published to as it stands.
    Existing,
}

/// The bounds of every wait the sink makes on the broker, and of the records it
/// holds unconfirmed. Each is a positive integer variable, unset takes the
/// default. A wait the drainer makes (connect, publish) is at most
/// [`MAX_DRAINER_WAIT_MS`]; the confirm and backoff waits, off the drainer, at
/// most [`MAX_WAIT_MS`]; the window at most [`MAX_WINDOW`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CdrDeliveryBounds {
    /// Publishes awaiting the broker's confirm at once,
    /// `B2BUA_CDR_RABBITMQ_WINDOW` (default 256). A record that finds the
    /// window full waits for a slot within `publish_timeout`.
    pub window: usize,
    /// TCP connect, AMQP handshake, channel open, `confirm.select` and queue
    /// declare together, `B2BUA_CDR_RABBITMQ_CONNECT_TIMEOUT_MS` (default 3000).
    pub connect_timeout: Duration,
    /// A window slot plus the hand-off of the publish to the connection,
    /// `B2BUA_CDR_RABBITMQ_PUBLISH_TIMEOUT_MS` (default 1000).
    pub publish_timeout: Duration,
    /// From the publish to the broker's confirm,
    /// `B2BUA_CDR_RABBITMQ_CONFIRM_TIMEOUT_MS` (default 5000).
    pub confirm_timeout: Duration,
    /// The first wait after a failed connection, doubled per consecutive
    /// failure, `B2BUA_CDR_RABBITMQ_BACKOFF_MS` (default 500).
    pub backoff_min: Duration,
    /// The longest wait between two connect attempts,
    /// `B2BUA_CDR_RABBITMQ_BACKOFF_MAX_MS` (default 5000, at least `_BACKOFF_MS`).
    pub backoff_max: Duration,
}

/// The longest wait any `_MS` bound states: one hour.
pub const MAX_WAIT_MS: u64 = 3_600_000;
/// The longest connect or publish bound: one minute.
pub const MAX_DRAINER_WAIT_MS: u64 = 60_000;
/// The largest publish window.
pub const MAX_WINDOW: u64 = 65_536;

impl Default for CdrDeliveryBounds {
    fn default() -> Self {
        Self {
            window: 256,
            connect_timeout: Duration::from_millis(3_000),
            publish_timeout: Duration::from_millis(1_000),
            confirm_timeout: Duration::from_millis(5_000),
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_millis(5_000),
        }
    }
}

impl CdrDeliveryBounds {
    fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = Self::default();
        let ms = |key: &str, default: Duration, max: u64| -> Result<Duration, String> {
            bounded(get, key, default.as_millis() as u64, max).map(Duration::from_millis)
        };
        let bounds = Self {
            window: bounded(get, "B2BUA_CDR_RABBITMQ_WINDOW", d.window as u64, MAX_WINDOW)?
                as usize,
            connect_timeout: ms(
                "B2BUA_CDR_RABBITMQ_CONNECT_TIMEOUT_MS",
                d.connect_timeout,
                MAX_DRAINER_WAIT_MS,
            )?,
            publish_timeout: ms(
                "B2BUA_CDR_RABBITMQ_PUBLISH_TIMEOUT_MS",
                d.publish_timeout,
                MAX_DRAINER_WAIT_MS,
            )?,
            confirm_timeout: ms(
                "B2BUA_CDR_RABBITMQ_CONFIRM_TIMEOUT_MS",
                d.confirm_timeout,
                MAX_WAIT_MS,
            )?,
            backoff_min: ms("B2BUA_CDR_RABBITMQ_BACKOFF_MS", d.backoff_min, MAX_WAIT_MS)?,
            backoff_max: ms("B2BUA_CDR_RABBITMQ_BACKOFF_MAX_MS", d.backoff_max, MAX_WAIT_MS)?,
        };
        if bounds.backoff_max < bounds.backoff_min {
            return Err(format!(
                "B2BUA_CDR_RABBITMQ_BACKOFF_MAX_MS ({} ms) must not be below \
                 B2BUA_CDR_RABBITMQ_BACKOFF_MS ({} ms)",
                bounds.backoff_max.as_millis(),
                bounds.backoff_min.as_millis()
            ));
        }
        Ok(bounds)
    }
}

/// The integer in `1..=max` that `key` states, `default` when unset.
fn bounded(
    get: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: u64,
    max: u64,
) -> Result<u64, String> {
    let Some(raw) = get(key) else {
        return Ok(default);
    };
    match raw.parse::<u64>() {
        Ok(v) if (1..=max).contains(&v) => Ok(v),
        _ => Err(format!("{key} must be an integer from 1 to {max}, got {raw:?}")),
    }
}

/// The RabbitMQ CDR sink's env grammar, the one statement of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RabbitMqCdrSettings {
    /// AMQP URI (`amqp://user:pass@host:5672/vhost`), `B2BUA_CDR_RABBITMQ_URL`.
    /// The URI's own `connection_timeout` is not read: `connect_timeout`
    /// bounds the connect.
    pub url: String,
    /// Destination queue, `B2BUA_CDR_RABBITMQ_QUEUE` (default `cdr`).
    pub queue: String,
    /// How the queue is declared, `B2BUA_CDR_RABBITMQ_DECLARE` + `_MAX_LEN`.
    pub declare: CdrQueueDeclare,
    /// The bounds of every broker wait, `B2BUA_CDR_RABBITMQ_WINDOW` and the
    /// `_*_TIMEOUT_MS` / `_BACKOFF*_MS` variables.
    pub bounds: CdrDeliveryBounds,
}

impl RabbitMqCdrSettings {
    /// Reads the grammar through `get`; `Ok(None)` when the URL is unset or
    /// blank. `Err` names the variable when `B2BUA_CDR_QUEUE` is `0` (no
    /// buffer between the call path and the broker), when `QUEUE` is blank or
    /// padded, when `MAX_LEN`
    /// is not a non-negative integer, when `DECLARE` is neither `own` nor
    /// `existing` (blank = `own`), when `MAX_LEN` is set beside
    /// `DECLARE=existing` (the broker owns the bound), or when a bound of
    /// [`CdrDeliveryBounds`] is not a positive integer.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let Some(url) = get("B2BUA_CDR_RABBITMQ_URL").filter(|u| !u.trim().is_empty()) else {
            return Ok(None);
        };
        let queue = get("B2BUA_CDR_RABBITMQ_QUEUE").unwrap_or_else(|| "cdr".to_string());
        if queue.trim().is_empty() || queue.trim() != queue {
            return Err(format!(
                "B2BUA_CDR_RABBITMQ_QUEUE must name a queue with no surrounding whitespace, \
                 got {queue:?}; unset it for `cdr`"
            ));
        }
        if get("B2BUA_CDR_QUEUE").and_then(|q| q.parse::<usize>().ok()) == Some(0) {
            return Err("B2BUA_CDR_QUEUE=0 calls the CDR sink inline on the call path; with \
                 B2BUA_CDR_RABBITMQ_URL set every broker wait would stall calls: give the \
                 CDR buffer a depth"
                .to_string());
        }
        let raw_max = get("B2BUA_CDR_RABBITMQ_MAX_LEN");
        let raw_declare = get("B2BUA_CDR_RABBITMQ_DECLARE").filter(|v| !v.trim().is_empty());
        let declare = match raw_declare.as_deref() {
            None | Some("own") => {
                let raw_max = raw_max.unwrap_or_else(|| "100000".to_string());
                let max_len = raw_max.parse::<i64>().ok().filter(|m| *m >= 0).ok_or_else(|| {
                    format!(
                        "B2BUA_CDR_RABBITMQ_MAX_LEN must be a non-negative integer (0 = \
                         unbounded), got {raw_max:?}"
                    )
                })?;
                CdrQueueDeclare::Own { max_len }
            }
            Some("existing") => {
                if let Some(raw_max) = raw_max {
                    return Err(format!(
                        "B2BUA_CDR_RABBITMQ_MAX_LEN={raw_max:?} has no effect with \
                         B2BUA_CDR_RABBITMQ_DECLARE=existing: the broker holds the queue \
                         and its arguments; unset it"
                    ));
                }
                CdrQueueDeclare::Existing
            }
            Some(other) => {
                return Err(format!(
                    "B2BUA_CDR_RABBITMQ_DECLARE must be `own` or `existing`, got {other:?}"
                ));
            }
        };
        let bounds = CdrDeliveryBounds::from_lookup(&get)?;
        Ok(Some(Self { url, queue, declare, bounds }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    fn with_url(pairs: &[(&str, &str)]) -> Result<Option<RabbitMqCdrSettings>, String> {
        let mut all = vec![("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f")];
        all.extend_from_slice(pairs);
        RabbitMqCdrSettings::from_lookup(lookup(&all))
    }

    const OWN_DEFAULT: CdrQueueDeclare = CdrQueueDeclare::Own { max_len: 100_000 };

    #[test]
    fn unset_or_blank_url_selects_no_sink() {
        assert_eq!(RabbitMqCdrSettings::from_lookup(lookup(&[])), Ok(None));
        assert_eq!(
            RabbitMqCdrSettings::from_lookup(lookup(&[("B2BUA_CDR_RABBITMQ_URL", "  ")])),
            Ok(None)
        );
    }

    #[test]
    fn url_alone_takes_the_default_queue_owns_it_bounded_and_takes_the_default_bounds() {
        let s = RabbitMqCdrSettings::from_lookup(lookup(&[(
            "B2BUA_CDR_RABBITMQ_URL",
            "amqp://guest:guest@rabbitmq:5672/%2f",
        )]));
        assert_eq!(
            s,
            Ok(Some(RabbitMqCdrSettings {
                url: "amqp://guest:guest@rabbitmq:5672/%2f".into(),
                queue: "cdr".into(),
                declare: OWN_DEFAULT,
                bounds: CdrDeliveryBounds::default(),
            }))
        );
    }

    #[test]
    fn the_default_bounds_are_the_documented_ones() {
        let d = CdrDeliveryBounds::default();
        assert_eq!(d.window, 256);
        assert_eq!(
            [d.connect_timeout, d.publish_timeout, d.confirm_timeout, d.backoff_min, d.backoff_max]
                .map(|t| t.as_millis()),
            [3_000, 1_000, 5_000, 500, 5_000]
        );
    }

    #[test]
    fn queue_and_bound_are_read_from_their_variables() {
        let s = with_url(&[
            ("B2BUA_CDR_RABBITMQ_QUEUE", "cdr-lane"),
            ("B2BUA_CDR_RABBITMQ_MAX_LEN", "0"),
        ])
        .expect("ok")
        .expect("some");
        assert_eq!(
            (s.queue.as_str(), s.declare),
            ("cdr-lane", CdrQueueDeclare::Own { max_len: 0 })
        );
    }

    #[test]
    fn a_blank_queue_is_refused_naming_its_variable() {
        for v in ["", "  "] {
            let e = with_url(&[("B2BUA_CDR_RABBITMQ_QUEUE", v)])
                .expect_err("a blank queue publishes to nowhere and must refuse boot");
            assert!(e.contains("B2BUA_CDR_RABBITMQ_QUEUE"), "msg was: {e}");
        }
    }

    #[test]
    fn a_bound_that_is_not_a_non_negative_integer_is_refused_naming_its_variable() {
        for v in ["lots", " 5", "-1", "-100000"] {
            let e = with_url(&[("B2BUA_CDR_RABBITMQ_MAX_LEN", v)])
                .expect_err("a non-integer or negative bound must refuse boot");
            assert!(e.contains("B2BUA_CDR_RABBITMQ_MAX_LEN"), "MAX_LEN={v:?}: {e}");
        }
    }

    #[test]
    fn declare_own_and_blank_select_the_owned_queue() {
        for v in ["own", "", "  "] {
            let s =
                with_url(&[("B2BUA_CDR_RABBITMQ_DECLARE", v), ("B2BUA_CDR_RABBITMQ_MAX_LEN", "7")])
                    .expect("ok")
                    .expect("some");
            assert_eq!(s.declare, CdrQueueDeclare::Own { max_len: 7 }, "DECLARE={v:?}");
        }
    }

    #[test]
    fn declare_existing_selects_the_broker_held_queue() {
        let s = with_url(&[
            ("B2BUA_CDR_RABBITMQ_QUEUE", "held"),
            ("B2BUA_CDR_RABBITMQ_DECLARE", "existing"),
        ])
        .expect("ok")
        .expect("some");
        assert_eq!((s.queue.as_str(), s.declare), ("held", CdrQueueDeclare::Existing));
    }

    #[test]
    fn an_unknown_declare_is_refused_naming_its_variable_and_values() {
        for v in ["passive", "Own", " own"] {
            let e = with_url(&[("B2BUA_CDR_RABBITMQ_DECLARE", v)])
                .expect_err("an unknown declare policy must refuse boot");
            assert!(e.contains("B2BUA_CDR_RABBITMQ_DECLARE"), "msg was: {e}");
            assert!(e.contains("own") && e.contains("existing"), "msg was: {e}");
        }
    }

    #[test]
    fn a_bound_beside_declare_existing_is_refused_naming_both_variables() {
        let e = with_url(&[
            ("B2BUA_CDR_RABBITMQ_DECLARE", "existing"),
            ("B2BUA_CDR_RABBITMQ_MAX_LEN", "100000"),
        ])
        .expect_err("a bound on a broker-held queue has no effect and must refuse boot");
        assert!(e.contains("B2BUA_CDR_RABBITMQ_MAX_LEN"), "msg was: {e}");
        assert!(e.contains("B2BUA_CDR_RABBITMQ_DECLARE"), "msg was: {e}");
    }

    #[test]
    fn every_delivery_bound_is_read_from_its_variable() {
        let s = with_url(&[
            ("B2BUA_CDR_RABBITMQ_WINDOW", "8"),
            ("B2BUA_CDR_RABBITMQ_CONNECT_TIMEOUT_MS", "11"),
            ("B2BUA_CDR_RABBITMQ_PUBLISH_TIMEOUT_MS", "12"),
            ("B2BUA_CDR_RABBITMQ_CONFIRM_TIMEOUT_MS", "13"),
            ("B2BUA_CDR_RABBITMQ_BACKOFF_MS", "14"),
            ("B2BUA_CDR_RABBITMQ_BACKOFF_MAX_MS", "15"),
        ])
        .expect("ok")
        .expect("some");
        let ms = Duration::from_millis;
        assert_eq!(
            s.bounds,
            CdrDeliveryBounds {
                window: 8,
                connect_timeout: ms(11),
                publish_timeout: ms(12),
                confirm_timeout: ms(13),
                backoff_min: ms(14),
                backoff_max: ms(15),
            }
        );
    }

    #[test]
    fn a_delivery_bound_out_of_its_range_is_refused_naming_its_variable() {
        for key in [
            "B2BUA_CDR_RABBITMQ_WINDOW",
            "B2BUA_CDR_RABBITMQ_CONNECT_TIMEOUT_MS",
            "B2BUA_CDR_RABBITMQ_PUBLISH_TIMEOUT_MS",
            "B2BUA_CDR_RABBITMQ_CONFIRM_TIMEOUT_MS",
            "B2BUA_CDR_RABBITMQ_BACKOFF_MS",
            "B2BUA_CDR_RABBITMQ_BACKOFF_MAX_MS",
        ] {
            let too_big = if key.ends_with("_WINDOW") {
                MAX_WINDOW + 1
            } else if key.ends_with("_CONNECT_TIMEOUT_MS") || key.ends_with("_PUBLISH_TIMEOUT_MS") {
                60_001
            } else {
                3_600_001
            };
            for v in ["0", "-1", "soon", "", " 5", &too_big.to_string()] {
                let e = with_url(&[(key, v)]).expect_err("an unbounded or unreadable wait");
                assert!(e.contains(key), "{key}={v:?}: {e}");
            }
        }
    }

    #[test]
    fn a_backoff_ceiling_below_its_floor_is_refused_naming_both_variables() {
        let e = with_url(&[
            ("B2BUA_CDR_RABBITMQ_BACKOFF_MS", "2000"),
            ("B2BUA_CDR_RABBITMQ_BACKOFF_MAX_MS", "1000"),
        ])
        .expect_err("a ceiling below the floor");
        assert!(e.contains("B2BUA_CDR_RABBITMQ_BACKOFF_MS"), "msg was: {e}");
        assert!(e.contains("B2BUA_CDR_RABBITMQ_BACKOFF_MAX_MS"), "msg was: {e}");
    }

    #[test]
    fn the_drainer_waits_are_capped_at_a_minute_and_the_others_at_an_hour() {
        let s = with_url(&[
            ("B2BUA_CDR_RABBITMQ_CONNECT_TIMEOUT_MS", "60000"),
            ("B2BUA_CDR_RABBITMQ_PUBLISH_TIMEOUT_MS", "60000"),
            ("B2BUA_CDR_RABBITMQ_CONFIRM_TIMEOUT_MS", "3600000"),
            ("B2BUA_CDR_RABBITMQ_BACKOFF_MAX_MS", "3600000"),
        ])
        .expect("ok")
        .expect("some");
        assert_eq!(s.bounds.connect_timeout, Duration::from_secs(60));
        assert_eq!(s.bounds.confirm_timeout, Duration::from_secs(3_600));
    }

    #[test]
    fn a_queue_name_with_surrounding_whitespace_is_refused_naming_its_variable() {
        for v in [" cdr", "cdr ", "\tcdr"] {
            let e = with_url(&[("B2BUA_CDR_RABBITMQ_QUEUE", v)]).expect_err("padded queue name");
            assert!(e.contains("B2BUA_CDR_RABBITMQ_QUEUE"), "{v:?}: {e}");
        }
    }

    /// A zero `B2BUA_CDR_QUEUE` makes the CDR buffer a passthrough, which would
    /// put every broker wait on the call path.
    #[test]
    fn an_unbuffered_cdr_path_beside_a_broker_is_refused_naming_both_variables() {
        for v in ["0", "00"] {
            let e = with_url(&[("B2BUA_CDR_QUEUE", v)]).expect_err("an unbuffered broker sink");
            assert!(e.contains("B2BUA_CDR_QUEUE"), "{v:?}: {e}");
            assert!(e.contains("B2BUA_CDR_RABBITMQ_URL"), "{v:?}: {e}");
        }
        assert!(with_url(&[("B2BUA_CDR_QUEUE", "8")]).expect("ok").is_some());
        assert_eq!(
            RabbitMqCdrSettings::from_lookup(lookup(&[("B2BUA_CDR_QUEUE", "0")])),
            Ok(None),
            "without a broker a passthrough blocks on nothing"
        );
    }
}
