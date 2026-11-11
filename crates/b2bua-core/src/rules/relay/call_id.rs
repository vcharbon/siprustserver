//! The Call-ID of a leg the B2BUA dials: `<N>-<originator's Call-ID>`, `N`
//! the leg's [`leg_call_id_number`](call::helpers::leg_call_id_number). A
//! capture joins every leg of a call on the originator's Call-ID with
//! `^\d+-(.+)$`. Distinct within one copy of a call; a re-offered Call-ID mints
//! the same ids again (ADR-0059).

use sip_message::header::HeaderName;
use sip_message::FirstIngressBound;

/// The Call-ID of leg `leg_id`, dialled for the call the originator opened as
/// `originator_call_id`. `None` when the leg id carries no number or the
/// `Call-ID` line would exceed `bound`, the length the next hop reads a new
/// request to: the caller then mints a Call-ID of its own.
pub(crate) fn dialled_call_id(
    leg_id: &str,
    originator_call_id: &str,
    bound: &FirstIngressBound,
) -> Option<String> {
    let n = call::helpers::leg_call_id_number(leg_id)?;
    let call_id = format!("{n}-{originator_call_id}");
    let line = HeaderName::CallId.as_wire_str().len() + 2 + call_id.len();
    (line <= bound.header_length).then_some(call_id)
}
