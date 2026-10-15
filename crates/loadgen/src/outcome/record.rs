//! A placed call's `call-outcome.v1` record: the caller's facts from the wire
//! ([`WireFacts`]) and the failures loadgen's own result adds, never a
//! judgement. The result class rides in `producer_verdict` only.

use serde_json::{json, Value};

use crate::class::{CallOutcome, ResultClass};
use crate::outcome::wire::WireFacts;
use scenario_harness::StepError;

/// The record schema this module writes.
pub const SCHEMA: &str = "call-outcome.v1";

/// Who wrote a record and for which run, stream and shape.
#[derive(Debug, Clone)]
pub struct RecordMeta<'a> {
    pub run: &'a str,
    pub source: &'a str,
    pub stream: &'a str,
    pub shape: &'a str,
}

/// The bound on a failure's `detail` text.
const DETAIL_MAX: usize = 160;

/// One `failures` entry.
fn failure(at_ms: i64, kind: &str, detail: Option<String>) -> Value {
    let mut f = json!({ "at_ms": at_ms, "kind": kind });
    if let Some(mut d) = detail {
        if d.len() > DETAIL_MAX {
            let cut = (0..=DETAIL_MAX).rev().find(|&i| d.is_char_boundary(i)).unwrap_or(0);
            d.truncate(cut);
        }
        f["detail"] = json!(d);
    }
    f
}

/// How loadgen's own result stopped or marked the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// It ended on an accepted branch.
    Ok,
    /// A wait passed with nothing.
    Timeout,
    Transport,
    /// An unexpected or malformed message stopped the call.
    Protocol,
    /// A post-call oracle (RFC audit, case checks) flagged a call that ran.
    Flagged,
    /// The generator itself failed.
    Generator,
}

fn stop_of(outcome: &CallOutcome) -> Stop {
    match outcome {
        CallOutcome::Ok { .. } => Stop::Ok,
        CallOutcome::Panic(_) | CallOutcome::Rejected(_) => Stop::Generator,
        CallOutcome::RfcAuditFail(_) | CallOutcome::CheckFail(_) => Stop::Flagged,
        CallOutcome::Step { cause, .. } => match cause {
            StepError::Timeout { .. } | StepError::QueueClosed { .. } => Stop::Timeout,
            StepError::Transport { .. } => Stop::Transport,
            // A final other than the step expected: the wire states it.
            StepError::WrongStatus { .. } => Stop::Flagged,
            _ => Stop::Protocol,
        },
    }
}

/// The failures loadgen's result adds to the wire's, at `at_ms`.
fn result_failures(outcome: &CallOutcome, at_ms: i64, stop: Stop) -> Vec<Value> {
    match outcome {
        CallOutcome::Panic(m) => vec![failure(at_ms, "generator", Some(format!("panic: {m}")))],
        CallOutcome::RfcAuditFail(findings) => findings
            .iter()
            .take(8)
            .map(|f| failure(at_ms, "protocol", Some(f.rule.clone())))
            .collect(),
        CallOutcome::CheckFail(failed) => failed
            .iter()
            .take(8)
            .map(|v| failure(at_ms, "protocol", Some(format!("check {} {}", v.on, v.field))))
            .collect(),
        CallOutcome::Step { at, cause } => match stop {
            Stop::Transport => {
                vec![failure(at_ms, "transport", Some(format!("{cause} (at {at})")))]
            }
            Stop::Protocol => vec![failure(at_ms, "protocol", Some(format!("{cause} (at {at})")))],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The record of a placed call ended at `ended_at_ms` with `outcome`; `None`
/// for a call never placed (refused before any datagram, or no INVITE sent).
pub fn build(
    meta: &RecordMeta<'_>,
    wire: &WireFacts,
    outcome: &CallOutcome,
    ended_at_ms: i64,
) -> Option<Value> {
    let (Some(call_id), Some(started)) = (wire.call_id.as_deref(), wire.started_at_ms) else {
        return None;
    };
    if matches!(outcome, CallOutcome::Rejected(_)) {
        return None;
    }
    let stop = stop_of(outcome);
    let mut failures: Vec<Value> = wire
        .failures
        .iter()
        .map(|f| {
            let mut v = failure(f.at_ms, "dialog_request", None);
            v["method"] = json!(f.method);
            v["code"] = json!(f.code);
            v
        })
        .collect();
    failures.extend(result_failures(outcome, ended_at_ms, stop));
    let answered = wire.answered_at_ms.is_some();
    let mut setup_ended = wire.setup_ended_at_ms;
    let (ending, reason, phase) = if !answered {
        let phase = if wire.first_provisional_at_ms.is_some() { "provisional" } else { "invited" };
        match wire.final_code {
            Some(487) if wire.cancelled => ("abandoned", Some("487".to_string()), phase),
            Some(code) => {
                let reason = wire.final_cause.clone().unwrap_or_else(|| code.to_string());
                ("rejected", Some(reason), phase)
            }
            None => match stop {
                Stop::Transport => ("failed", Some("transport".to_string()), phase),
                Stop::Protocol | Stop::Generator => ("failed", Some("protocol".to_string()), phase),
                // No final by the time the caller stopped waiting.
                Stop::Ok | Stop::Timeout | Stop::Flagged => {
                    setup_ended = Some(ended_at_ms);
                    ("timeout", Some("timeout".to_string()), phase)
                }
            },
        }
    } else {
        let released = matches!(wire.bye, Some(Some(c)) if c < 300)
            || matches!(wire.bye_received, Some(Some(c)) if c < 300);
        let ending = if stop == Stop::Generator {
            "failed"
        } else if wire.saw_481 || (wire.bye_received.is_some() && stop != Stop::Ok) {
            "cut"
        } else if stop == Stop::Ok || (released && stop != Stop::Timeout) {
            "answered"
        } else if matches!(stop, Stop::Transport | Stop::Protocol) && !released {
            "failed"
        } else {
            "stranded"
        };
        let phase = if ending == "answered" || released { "released" } else { "answered" };
        (ending, None, phase)
    };
    if ending == "stranded" && stop == Stop::Timeout && wire.bye.is_none() {
        failures.push(failure(ended_at_ms, "dialog_request", Some("no final".to_string())));
    }
    let legs: Vec<Value> = wire
        .legs
        .iter()
        .map(|l| {
            let ending = match l.final_code {
                Some(c) if c < 300 => "answered",
                Some(487) if l.cancelled => "abandoned",
                Some(_) => "rejected",
                None => "timeout",
            };
            json!({
                "role": l.role,
                "ending": ending,
                "final_code": l.final_code,
                "early_media": l.early_media,
            })
        })
        .collect();
    let verdict = match outcome {
        CallOutcome::Ok { branch } => format!("ok:{branch}"),
        other => ResultClass::from(other).label(),
    };
    Some(json!({
        "schema": SCHEMA,
        "run": meta.run,
        "source": meta.source,
        "stream": meta.stream,
        "shape": meta.shape,
        "call_id": call_id,
        "started_at_ms": started,
        "first_provisional_at_ms": wire.first_provisional_at_ms,
        "setup_ended_at_ms": setup_ended,
        "answered_at_ms": wire.answered_at_ms,
        "ended_at_ms": ended_at_ms,
        "phase_reached": phase,
        "early_media": wire.early_media,
        "final_code": wire.final_code,
        "reason": reason,
        "ending": ending,
        "failures": failures,
        "legs": legs,
        "producer_verdict": verdict,
    }))
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
