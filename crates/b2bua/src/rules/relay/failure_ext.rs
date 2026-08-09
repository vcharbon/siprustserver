//! The relayed-failure-headers `Call.ext` slot: the relayable header image of
//! the failure round trip in flight (the `/call/failure` consult), folded only
//! into the a-facing final that answers that consult (ADR-0017 X2). A reserved
//! core slot rides replicated call state but never reaches a decision backend
//! (ADR-0016).

use sip_message::{SipHeader as MsgHeader, SipStr};

use super::passthrough::relay_response_passthrough_headers;

/// `Call.ext` slot carrying the relayable header image of the failure round
/// trip IN FLIGHT — the `/call/failure` consult the caller is still waiting on.
/// **Every** consult restates it, empty when that failure produced no peer
/// final (a no-answer or transaction timeout), so a superseded attempt's
/// headers can never outlive their own failure; an answer (`confirm-dialog`)
/// clears it, so an established call replicates none of it. Only the final
/// answering that consult folds it, under the decision's `header_updates`
/// (ADR-0017 X2).
pub const RELAYED_FAILURE_HEADERS_EXT: &str = "relayed-failure-headers";

/// Is this `Call.ext` key the CORE's own slot rather than a service id? A
/// reserved key rides the replicated call state but is never a service slice,
/// so it never reaches a decision backend (ADR-0016).
pub fn is_core_reserved_ext(key: &str) -> bool {
    key == RELAYED_FAILURE_HEADERS_EXT
}

/// The one-entry `Call.ext` merge every `/call/failure` consult states: the
/// failing final's relayable image for the [`RELAYED_FAILURE_HEADERS_EXT`]
/// slot — a JSON array of `[name, value]` pairs, wire order and repeats kept,
/// body dropped ([`relay_response_passthrough_headers`], since the minted final
/// never carries the source's body) — or JSON null, which CLEARS the slot, when
/// the failure has no peer final to state.
pub fn failure_headers_ext(resp: Option<&sip_message::SipResponse>) -> call::ExtMap {
    let value = match resp {
        Some(resp) => {
            let pairs: Vec<serde_json::Value> = relay_response_passthrough_headers(resp, false)
                .iter()
                .map(|h| serde_json::json!([h.name.as_str(), h.value.as_str()]))
                .collect();
            serde_json::Value::Array(pairs)
        }
        None => serde_json::Value::Null,
    };
    let mut ext = call::ExtMap::new();
    ext.insert(RELAYED_FAILURE_HEADERS_EXT.to_string(), value);
    ext
}

/// Decode the [`RELAYED_FAILURE_HEADERS_EXT`] slot back into headers. Empty
/// when the failure round trip in flight produced no peer final, and empty on
/// every a-facing final that answers something else (a setup deadline, a
/// capacity refusal, a media-service failure) rather than that round trip.
pub fn relayed_failure_headers(ext: Option<&call::ExtMap>) -> Vec<MsgHeader> {
    ext.and_then(|m| m.get(RELAYED_FAILURE_HEADERS_EXT))
        .and_then(|v| v.as_array())
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|p| {
                    let name = p.get(0)?.as_str()?;
                    let value = p.get(1)?.as_str()?;
                    Some(MsgHeader { name: SipStr::owned(name), value: SipStr::owned(value) })
                })
                .collect()
        })
        .unwrap_or_default()
}
