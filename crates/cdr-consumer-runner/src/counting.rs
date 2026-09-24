//! What the consumer counts off each delivery, per the payload mode the
//! `CDR_PAYLOAD` variable selects, and the Prometheus text it exposes.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;

/// How deliveries are read, `CDR_PAYLOAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// `json` (default; unset or blank): each delivery is a JSON record with
    /// `created_at` and `terminated_at` (ms). It counts one consumed record
    /// and adds its duration; a delivery that does not decode counts one parse
    /// error and no consumed record.
    Json,
    /// `opaque`: every delivery counts one consumed record, whatever its
    /// bytes; nothing is decoded and no duration is kept.
    Opaque,
}

impl Payload {
    /// The mode `raw` names; `Err` for any other value.
    pub fn from_env_value(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim) {
            None | Some("") | Some("json") => Ok(Payload::Json),
            Some("opaque") => Ok(Payload::Opaque),
            Some(other) => Err(format!("CDR_PAYLOAD must be `json` or `opaque`, got {other:?}")),
        }
    }
}

/// The only fields read off a JSON CDR; serde_json ignores the rest.
#[derive(Deserialize)]
struct CdrDurationView {
    created_at: i64,
    terminated_at: i64,
}

#[derive(Default)]
pub struct Metrics {
    consumed: AtomicU64,
    duration_ms: AtomicU64,
    parse_errors: AtomicU64,
}

impl Metrics {
    /// Counts one delivery of `data`; `Err` carries a JSON decode error.
    pub fn count(&self, payload: Payload, data: &[u8]) -> Result<(), serde_json::Error> {
        match payload {
            Payload::Opaque => {
                self.consumed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Payload::Json => match serde_json::from_slice::<CdrDurationView>(data) {
                Ok(view) => {
                    let dur = (view.terminated_at - view.created_at).max(0) as u64;
                    self.consumed.fetch_add(1, Ordering::Relaxed);
                    self.duration_ms.fetch_add(dur, Ordering::Relaxed);
                    Ok(())
                }
                Err(e) => {
                    self.parse_errors.fetch_add(1, Ordering::Relaxed);
                    Err(e)
                }
            },
        }
    }

    /// The exposition of the counters `payload` keeps: `cdr_consumed_total`
    /// always, the duration and parse-error counters in `json` mode only.
    pub fn prometheus_text(&self, payload: Payload) -> String {
        let mut s = String::new();
        s.push_str("# HELP cdr_consumed_total total CDRs consumed from the RabbitMQ queue\n");
        s.push_str("# TYPE cdr_consumed_total counter\n");
        s.push_str(&format!("cdr_consumed_total {}\n", self.consumed.load(Ordering::Relaxed)));
        if payload == Payload::Json {
            s.push_str("# HELP cdr_call_duration_ms_total summed call duration in ms across all consumed CDRs (terminated_at - created_at)\n");
            s.push_str("# TYPE cdr_call_duration_ms_total counter\n");
            s.push_str(&format!(
                "cdr_call_duration_ms_total {}\n",
                self.duration_ms.load(Ordering::Relaxed)
            ));
            s.push_str("# HELP cdr_parse_errors_total CDR payloads that failed to decode\n");
            s.push_str("# TYPE cdr_parse_errors_total counter\n");
            s.push_str(&format!(
                "cdr_parse_errors_total {}\n",
                self.parse_errors.load(Ordering::Relaxed)
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, name: &str) -> Option<String> {
        text.lines().find(|l| l.starts_with(&format!("{name} "))).map(str::to_string)
    }

    #[test]
    fn unset_blank_and_json_select_json_opaque_selects_opaque() {
        for raw in [None, Some(""), Some("  "), Some("json")] {
            assert_eq!(Payload::from_env_value(raw), Ok(Payload::Json), "{raw:?}");
        }
        assert_eq!(Payload::from_env_value(Some("opaque")), Ok(Payload::Opaque));
    }

    #[test]
    fn an_unknown_mode_is_refused_naming_the_variable_and_its_values() {
        let e = Payload::from_env_value(Some("xml")).expect_err("unknown mode");
        assert!(e.contains("CDR_PAYLOAD") && e.contains("json") && e.contains("opaque"), "{e}");
    }

    #[test]
    fn json_counts_a_decoded_record_and_its_duration_and_a_parse_error_otherwise() {
        let m = Metrics::default();
        m.count(Payload::Json, br#"{"created_at":1000,"terminated_at":3500,"x":1}"#).unwrap();
        assert!(m.count(Payload::Json, b"<cdr/>").is_err());
        let text = m.prometheus_text(Payload::Json);
        assert_eq!(line(&text, "cdr_consumed_total").as_deref(), Some("cdr_consumed_total 1"));
        assert_eq!(
            line(&text, "cdr_call_duration_ms_total").as_deref(),
            Some("cdr_call_duration_ms_total 2500")
        );
        assert_eq!(
            line(&text, "cdr_parse_errors_total").as_deref(),
            Some("cdr_parse_errors_total 1")
        );
    }

    #[test]
    fn opaque_counts_every_delivery_whatever_its_bytes_and_keeps_no_duration() {
        let m = Metrics::default();
        for data in [&b"<cdr/>"[..], b"", br#"{"created_at":1000,"terminated_at":3500}"#, b"\xff"] {
            m.count(Payload::Opaque, data).unwrap();
        }
        let text = m.prometheus_text(Payload::Opaque);
        assert_eq!(line(&text, "cdr_consumed_total").as_deref(), Some("cdr_consumed_total 4"));
        assert_eq!(line(&text, "cdr_call_duration_ms_total"), None);
        assert_eq!(line(&text, "cdr_parse_errors_total"), None);
    }
}
