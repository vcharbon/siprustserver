//! The non-fatal header readings of an accepted message that came back as an
//! error: the optional structured headers the parser stores as `Err`, and the
//! route sets and RFC 6442 location headers read on access. Ingest counts and
//! copies the messages carrying one beside the ones the parser refuses;
//! `sipflow --reparse` reports each.

use sip_message::header::{Geolocation, GeolocationError, GeolocationRouting, HeaderName};
use sip_message::{SipMessage, SipParseError};

/// One header of an accepted message whose reading failed: its name, the
/// reason, and its values as written.
pub struct HeaderError {
    pub name: HeaderName,
    pub reason: String,
    pub values: Vec<String>,
}

/// Every failed non-fatal reading of `msg`, in a fixed header order.
pub fn of(msg: &SipMessage) -> Vec<HeaderError> {
    let o = msg.optional();
    let routes = match msg {
        SipMessage::Request(r) => (r.route_set().err(), r.record_route_set().err()),
        SipMessage::Response(r) => (r.route_set().err(), r.record_route_set().err()),
    };
    let geolocation = msg.list::<Geolocation>().err();
    let geolocation_error = msg.header::<GeolocationError>().and_then(Result::err);
    let geolocation_routing = msg.header::<GeolocationRouting>().and_then(Result::err);
    let readings: [(HeaderName, Option<&SipParseError>); 12] = [
        (HeaderName::PAssertedIdentity, o.p_asserted_identity.as_ref().err()),
        (HeaderName::PPreferredIdentity, o.p_preferred_identity.as_ref().err()),
        (HeaderName::Diversion, o.diversion.as_ref().err()),
        (HeaderName::HistoryInfo, o.history_info.as_ref().err()),
        (HeaderName::RemotePartyId, o.remote_party_id.as_ref().err()),
        (HeaderName::Geolocation, geolocation.as_ref()),
        (HeaderName::GeolocationError, geolocation_error.as_ref()),
        (HeaderName::GeolocationRouting, geolocation_routing.as_ref()),
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

#[cfg(test)]
mod tests {
    use sip_message::{CustomParser, SipParser};

    use super::*;

    fn invite(extra: &str) -> SipMessage {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-geo\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: geo@example.com\r\n\
CSeq: 1 INVITE\r\n\
{extra}Content-Length: 0\r\n\r\n"
        );
        CustomParser::new().parse(raw.as_bytes()).expect("a well-formed INVITE")
    }

    fn failed(msg: &SipMessage) -> Vec<String> {
        of(msg).into_iter().map(|e| format!("{}: {}", e.name, e.reason)).collect()
    }

    /// RFC 6442 §4.3: a Geolocation-Error is a code and parameters; §4.2: an
    /// unknown Geolocation-Routing value reads as "no". Neither is an error.
    #[test]
    fn rfc_6442_geolocation_values_read_without_error() {
        let msg = invite(
            "Geolocation: <cid:loc1@example.com>;inserted-by=192.0.2.1\r\n\
Geolocation-Error: 100;code=\"Cannot Process Location\"\r\n\
Geolocation-Routing: maybe\r\n",
        );
        assert_eq!(failed(&msg), Vec::<String>::new());
    }

    /// A Geolocation value outside its grammar is reported on read.
    #[test]
    fn a_malformed_geolocation_is_reported() {
        let msg = invite("Geolocation: cid:bare@example.com\r\nGeolocation-Error: busy\r\n");
        let names: Vec<HeaderName> = of(&msg).into_iter().map(|e| e.name).collect();
        assert_eq!(names, [HeaderName::Geolocation, HeaderName::GeolocationError]);
    }
}
