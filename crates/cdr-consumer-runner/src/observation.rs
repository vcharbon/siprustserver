//! A CDR reduced to what the HA tooling's CDR join compares: one
//! `cdr-observation.v1` line (ADR-0049 §3.8). A payload format is read by an
//! [`ObservationView`]; [`JsonRecordView`] reads the base's JSON record. A
//! product with another payload builds the consumer with its own view.

use serde::{Deserialize, Serialize};

/// One `cdr-observation.v1` line: times in Unix epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CdrObservation {
    pub schema: &'static str,
    /// The caller's Call-ID as the system under test received it.
    pub call_id: String,
    pub start_ms: i64,
    pub answer_ms: Option<i64>,
    pub end_ms: i64,
    /// The answered duration; 0 for a call never answered.
    pub duration_ms: i64,
    /// The resolution of the payload's own duration.
    pub duration_unit_ms: i64,
    /// `answered` or `not_answered`.
    pub disposition: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_code: Option<i64>,
}

impl CdrObservation {
    pub const SCHEMA: &'static str = "cdr-observation.v1";

    /// An observation from the instants a payload states; the duration runs
    /// from the answer to the end, 0 without an answer.
    pub fn new(
        call_id: String,
        start_ms: i64,
        answer_ms: Option<i64>,
        end_ms: i64,
        duration_unit_ms: i64,
    ) -> Self {
        CdrObservation {
            schema: Self::SCHEMA,
            call_id,
            start_ms,
            answer_ms,
            end_ms,
            duration_ms: answer_ms.map_or(0, |a| (end_ms - a).max(0)),
            duration_unit_ms,
            disposition: if answer_ms.is_some() { "answered" } else { "not_answered" },
            final_code: None,
        }
    }
}

/// Reads one delivery's payload; `None` when it holds no CDR this view reads.
pub trait ObservationView: Send + Sync {
    fn observe(&self, data: &[u8]) -> Option<CdrObservation>;
}

/// The base's JSON CDR record (`b2bua::cdr::CdrRecord`), read off the fields
/// the observation needs.
pub struct JsonRecordView;

#[derive(Deserialize)]
struct RecordView {
    created_at: i64,
    terminated_at: i64,
    a_leg: ALegView,
    #[serde(default)]
    events: Vec<EventView>,
}

#[derive(Deserialize)]
struct ALegView {
    call_id: String,
}

#[derive(Deserialize)]
struct EventView {
    #[serde(rename = "type")]
    event_type: String,
    timestamp: i64,
    reason: Option<String>,
}

impl ObservationView for JsonRecordView {
    /// The call was answered at its first `answer` event that carries no
    /// reason: the 2xx relayed to the caller. An answer with a reason is a
    /// callee's 2xx the record explains otherwise (one that crossed the
    /// caller's CANCEL and was ended with a BYE), never the caller's.
    fn observe(&self, data: &[u8]) -> Option<CdrObservation> {
        let r: RecordView = serde_json::from_slice(data).ok()?;
        let answer = r
            .events
            .iter()
            .find(|e| e.event_type == "answer" && e.reason.is_none())
            .map(|e| e.timestamp);
        Some(CdrObservation::new(r.a_leg.call_id, r.created_at, answer, r.terminated_at, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(events: &str) -> Vec<u8> {
        format!(
            r#"{{"call_ref":"r","created_at":1000,"terminated_at":31000,"a_leg":{{"call_id":"c1","from_tag":"f","state":"terminated"}},"b_legs":[],"events":[{events}],"decision_log":[],"termination":null}}"#
        )
        .into_bytes()
    }

    fn event(t: &str, at: i64, reason: Option<&str>) -> String {
        let reason = reason.map_or("null".to_string(), |r| format!("\"{r}\""));
        format!(
            r#"{{"type":"{t}","timestamp":{at},"leg_id":"b1","status_code":200,"reason":{reason},"decision_ordinal":1}}"#
        )
    }

    #[test]
    fn an_answered_record_is_observed_answered_with_the_duration_from_its_answer() {
        let data =
            record(&[event("invite_received", 1000, None), event("answer", 6000, None)].join(","));
        let o = JsonRecordView.observe(&data).expect("observed");
        assert_eq!(
            (o.call_id.as_str(), o.start_ms, o.answer_ms, o.end_ms, o.duration_ms, o.disposition),
            ("c1", 1000, Some(6000), 31000, 25000, "answered")
        );
        assert_eq!(o.duration_unit_ms, 1);
    }

    #[test]
    fn an_answer_with_a_reason_is_not_the_callers_answer() {
        let data = record(&event("answer", 6000, Some("cancel_crossing")));
        let o = JsonRecordView.observe(&data).expect("observed");
        assert_eq!((o.answer_ms, o.duration_ms, o.disposition), (None, 0, "not_answered"));
    }

    #[test]
    fn a_payload_that_is_no_record_is_not_observed() {
        assert!(JsonRecordView.observe(b"<cdr/>").is_none());
        assert!(JsonRecordView.observe(br#"{"created_at":1,"terminated_at":2}"#).is_none());
    }

    #[test]
    fn the_line_is_the_schema_s_shape() {
        let o = CdrObservation::new("c".into(), 1, None, 2, 1000);
        let v: serde_json::Value = serde_json::to_value(&o).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"schema":"cdr-observation.v1","call_id":"c","start_ms":1,"answer_ms":null,
                "end_ms":2,"duration_ms":0,"duration_unit_ms":1000,"disposition":"not_answered"})
        );
    }
}
