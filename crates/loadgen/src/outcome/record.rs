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
    let mut failures = dialog_failures(wire, ended_at_ms);
    failures.extend(result_failures(outcome, ended_at_ms, stop));
    let mut setup_ended = wire.setup_ended_at_ms;
    let (ending, reason, phase) = if wire.answered_at_ms.is_none() {
        let (ending, reason) = ending_before_answer(wire, stop);
        if ending == "timeout" {
            // No final by the time the caller stopped waiting.
            setup_ended = Some(ended_at_ms);
        }
        let phase = if wire.first_provisional_at_ms.is_some() { "provisional" } else { "invited" };
        (ending, Some(reason), phase)
    } else {
        let (ending, released) = ending_after_answer(wire, stop);
        // A step of the answered call that waited in vain: the call did not
        // run as its shape says, whatever its teardown drew. An in-dialog
        // request left unanswered already states why.
        if let (Stop::Timeout, CallOutcome::Step { at, .. }, true) =
            (stop, outcome, failures.is_empty())
        {
            failures.push(failure(ended_at_ms, "protocol", Some(format!("step {at} timed out"))));
        }
        if ending == "failed" && matches!(stop, Stop::Timeout | Stop::Flagged) {
            failures.push(failure(ended_at_ms, "protocol", Some("never released".into())));
        }
        (ending, None, if released { "released" } else { "answered" })
    };
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
        "retry_after_s": wire.final_retry_after_s,
        "ending": ending,
        "failures": failures,
        "legs": legs(wire),
        "producer_verdict": verdict,
    }))
}

/// The caller's failed in-dialog requests: each one answered non-2xx, then
/// each one (the BYE aside) still unanswered when the call ended.
fn dialog_failures(wire: &WireFacts, ended_at_ms: i64) -> Vec<Value> {
    let answered = wire.failures.iter().map(|f| (f, f.at_ms));
    let unanswered = wire.unanswered();
    let left = unanswered.iter().map(|f| (f, ended_at_ms));
    answered
        .chain(left)
        .map(|(f, at_ms)| {
            let mut v = failure(at_ms, "dialog_request", None);
            v["method"] = json!(f.method);
            if let Some(code) = f.code {
                v["code"] = json!(code);
            }
            v
        })
        .collect()
}

/// A call never answered: its final names the ending (`abandoned` for the
/// 487 to the caller's own CANCEL), else how loadgen stopped waiting. The
/// reason is the final's cause key, else its code, else the stop.
fn ending_before_answer(wire: &WireFacts, stop: Stop) -> (&'static str, String) {
    if let Some(code) = wire.final_code {
        let reason = wire.final_cause.clone().unwrap_or_else(|| code.to_string());
        let ending = if code == 487 && wire.cancelled { "abandoned" } else { "rejected" };
        return (ending, reason);
    }
    match stop {
        Stop::Transport => ("failed", "transport".into()),
        Stop::Protocol | Stop::Generator => ("failed", "protocol".into()),
        Stop::Ok | Stop::Timeout | Stop::Flagged => ("timeout", "timeout".into()),
    }
}

/// An answered call's ending and whether it reached `released` (a BYE of the
/// call drew a 2xx either way). A generator, protocol or transport stop is
/// `failed` whatever the teardown drew; a call ended on an accepted branch is
/// `answered`; otherwise a 481 or a BYE the caller did not wait for is `cut`,
/// the call's BYE without a final `stranded`, a dialog the call or the
/// generator's teardown released `answered`.
fn ending_after_answer(wire: &WireFacts, stop: Stop) -> (&'static str, bool) {
    let released = matches!(wire.bye, Some(Some(c)) if c < 300)
        || matches!(wire.bye_received, Some(Some(c)) if c < 300);
    let ending = match stop {
        Stop::Generator | Stop::Transport | Stop::Protocol => "failed",
        Stop::Ok => "answered",
        Stop::Timeout | Stop::Flagged if wire.saw_481 || wire.bye_received.is_some() => "cut",
        Stop::Timeout | Stop::Flagged if wire.bye == Some(None) => "stranded",
        Stop::Timeout | Stop::Flagged if released || wire.teardown_released => "answered",
        Stop::Timeout | Stop::Flagged => "failed",
    };
    (ending, released || ending == "answered")
}

/// Each callee leg's ending, from the final it sent before the verdict.
fn legs(wire: &WireFacts) -> Vec<Value> {
    wire.legs
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
        .collect()
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
