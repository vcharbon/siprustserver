//! The decision requests a callout sends, rebuilt from the JSON the seed
//! rule emitted.

use crate::decision::{CallFailureRequest, CallSnapshot, FailureInfo};

/// `[[name, value], …]` as emitted by a seed rule → header lines, wire order and
/// duplicates preserved; absent or malformed → empty.
fn parse_header_lines(v: Option<&serde_json::Value>) -> Vec<(String, String)> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|pair| {
                    let p = pair.as_array()?;
                    Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Rebuild a [`CallReferRequest`](crate::decision::CallReferRequest) from the
/// JSON the seed rule emitted.
pub(super) fn parse_call_refer_request(v: &serde_json::Value) -> crate::decision::CallReferRequest {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    crate::decision::CallReferRequest {
        call_id: s("call_id").unwrap_or_default(),
        dialog_id: s("dialog_id").unwrap_or_default(),
        callback_context: s("callback_context"),
        refer_to: s("refer_to").unwrap_or_default(),
        referred_by: s("referred_by"),
        sip_headers: parse_header_lines(v.get("sip_headers")),
        snapshot: CallSnapshot::default(),
    }
}

/// Rebuild a [`CallReleaseRequest`](crate::decision::CallReleaseRequest) from
/// the JSON the `max-duration` seed rule emitted, attaching the call-scoped
/// snapshot the dispatch site built from the authoritative call.
pub(super) fn parse_call_release_request(
    v: &serde_json::Value,
    snapshot: CallSnapshot,
) -> crate::decision::CallReleaseRequest {
    // The closed v1 set is the single `max_call_duration`; parse it via serde
    // (the enum's snake_case wire form) and default to it — the one event that
    // can currently emit this seed.
    let event = v
        .get("event")
        .and_then(|x| serde_json::from_value::<call::ReleaseEventKind>(x.clone()).ok())
        .unwrap_or(call::ReleaseEventKind::MaxCallDuration);
    crate::decision::CallReleaseRequest {
        callback_context: v.get("callback_context").and_then(|x| x.as_str()).map(str::to_string),
        event,
        snapshot,
    }
}

/// Rebuild a [`CallFailureRequest`] from the JSON the seed rule emitted. The
/// call-scoped `snapshot` is not part of the rule JSON — the dispatch site
/// attaches it from the authoritative call.
pub(super) fn parse_call_failure_request(v: &serde_json::Value) -> CallFailureRequest {
    CallFailureRequest {
        callback_context: v.get("callback_context").and_then(|x| x.as_str()).map(str::to_string),
        failure: FailureInfo {
            origin: v.get("origin").and_then(|x| x.as_str()).unwrap_or("external").to_string(),
            status_code: v
                .get("sip_code")
                .and_then(|x| x.as_u64())
                .and_then(|c| u16::try_from(c).ok()),
            limiter_id: v.get("limiter_id").and_then(|x| x.as_str()).map(str::to_string),
            failed_leg_id: v
                .get("failed_leg_id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            timeout_kind: v.get("timeout_kind").and_then(|x| x.as_str()).map(str::to_string),
            sip_headers: parse_header_lines(v.get("sip_headers")),
        },
        snapshot: CallSnapshot::default(),
    }
}
