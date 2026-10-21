//! The non-fatal header readings of an accepted message that came back as an
//! error: the optional structured headers the parser stores as `Err`, and the
//! route sets read on access.

use sip_message::header::HeaderName;
use sip_message::{SipMessage, SipParseError};

/// One header of an accepted message whose reading failed: its name, the
/// reason, and its values as written.
pub(crate) struct HeaderError {
    pub name: HeaderName,
    pub reason: String,
    pub values: Vec<String>,
}

/// Every failed non-fatal reading of `msg`, in a fixed header order.
pub(crate) fn of(msg: &SipMessage) -> Vec<HeaderError> {
    let o = msg.optional();
    let routes = match msg {
        SipMessage::Request(r) => (r.route_set().err(), r.record_route_set().err()),
        SipMessage::Response(r) => (r.route_set().err(), r.record_route_set().err()),
    };
    let readings: [(HeaderName, Option<&SipParseError>); 12] = [
        (HeaderName::PAssertedIdentity, o.p_asserted_identity.as_ref().err()),
        (HeaderName::PPreferredIdentity, o.p_preferred_identity.as_ref().err()),
        (HeaderName::Diversion, o.diversion.as_ref().err()),
        (HeaderName::HistoryInfo, o.history_info.as_ref().err()),
        (HeaderName::RemotePartyId, o.remote_party_id.as_ref().err()),
        (HeaderName::Geolocation, o.geolocation.as_ref().err()),
        (HeaderName::GeolocationError, o.geolocation_error.as_ref().err()),
        (HeaderName::GeolocationRouting, o.geolocation_routing.as_ref().err()),
        (HeaderName::RAck, o.rack.as_ref().err()),
        (HeaderName::ReferTo, o.refer_to.as_ref().err()),
        (HeaderName::Route, routes.0.as_ref()),
        (HeaderName::RecordRoute, routes.1.as_ref()),
    ];
    readings
        .into_iter()
        .filter_map(|(name, err)| {
            let reason = err?.reason.clone();
            let values = msg.raw(name.clone()).map(str::to_string).collect();
            Some(HeaderError { name, reason, values })
        })
        .collect()
}
