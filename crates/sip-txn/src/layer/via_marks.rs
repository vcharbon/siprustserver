//! The call attribution a request this node sends carries on its top Via:
//! what files a client transaction under its call, read back here.

use sip_message::header::ParamValue;
use sip_message::param_codec::decode_param;
use sip_message::SipRequest;

/// The call attribution a request's top Via carries.
pub(super) struct ViaMarks {
    /// `cr`: the callRef.
    pub(super) call_ref: Option<String>,
    /// `lg`: the leg id.
    pub(super) leg_id: Option<String>,
    /// `ci`: the mark of the call incarnation that sent it.
    pub(super) incarnation_mark: Option<String>,
}

/// The top Via's `cr` (callRef) / `lg` (legId) / `ci` (incarnation mark)
/// custom params, URL-decoded. The B2BUA's `build_call_via` URL-encodes them
/// (callRefs contain `|`/`@`) and a param value stays as written on the wire,
/// so decoding here is what makes `cancel_txns_for_call` match the natural
/// callRef the caller passes (see the cr/lg round-trip test).
pub(super) fn extract_via_custom_params(req: &SipRequest) -> ViaMarks {
    let top_via = req.top_via();
    let read = |name: &str| top_via.param(name).and_then(ParamValue::as_str).map(decode_param);
    ViaMarks { call_ref: read("cr"), leg_id: read("lg"), incarnation_mark: read("ci") }
}
