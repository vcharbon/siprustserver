//! The proxy's Record-Route policy: the entries this proxy records on a dialog
//! and the stickiness cookie read back off one, or off the proxy's own Via.
//!
//! Header *mechanics* — prepending a hop, popping an entry, stamping
//! received/rport — are sip-message draft vocabulary (ADR-0025). What rides the
//! proxy's OWN Record-Route is policy, and policy is what lives here.

use sip_message::char_class::{is_run_of, is_token_char};
use sip_message::header::{ParamValue, RecordRouteEntry, Uri, Via};
use sip_message::sip_str::SipStr;

use crate::addr::ProxyAddr;
use crate::strategy::{RouteParams, StatedOwners};

/// A loose-route Record-Route entry naming `advertised`: the stickiness params
/// in the order the strategy yields them, then the trailing `;lr`.
pub fn record_route<'a>(
    advertised: &ProxyAddr,
    params: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> RecordRouteEntry {
    entry(advertised, params, None)
}

/// [`record_route`] with a valueless flag param before `;lr` — the
/// worker-facing half of the double-Record-Route, which carries the `;outbound`
/// direction marker the in-dialog classification reads.
pub fn record_route_flagged<'a>(
    advertised: &ProxyAddr,
    params: impl IntoIterator<Item = (&'a String, &'a String)>,
    flag: &str,
) -> RecordRouteEntry {
    entry(advertised, params, Some(flag))
}

fn entry<'a>(
    advertised: &ProxyAddr,
    params: impl IntoIterator<Item = (&'a String, &'a String)>,
    flag: Option<&str>,
) -> RecordRouteEntry {
    let mut uri = Uri::sip(advertised.host.clone()).with_port(advertised.port);
    for (name, value) in params {
        // `pvalue = 1*paramchar` (RFC 3261 §25.1): an empty value rides as a
        // flag, which [`cookie_params`] reads back empty.
        let value =
            if value.is_empty() { ParamValue::Flag } else { ParamValue::text(value.as_str()) };
        uri = uri.with_param(name.as_str(), value);
    }
    if let Some(flag) = flag {
        uri = uri.with_flag(flag);
    }
    RecordRouteEntry::from_uri(uri.with_flag("lr"))
}

/// The stickiness cookie carried on one of the proxy's own Route /
/// Record-Route URIs. Names fold to lower case: the cookie is written by
/// `encode_stickiness` and read back by `decode_stickiness`, which spell their
/// fields one way.
pub fn cookie_params(uri: &Uri) -> RouteParams {
    uri.params()
        .iter()
        .map(|(name, value)| {
            (name.as_str().to_ascii_lowercase(), value.as_str().unwrap_or_default().to_string())
        })
        .collect()
}

/// The stickiness cookie carried on one of the proxy's own Vias, read as
/// [`cookie_params`] reads a URI: the Via's other params (branch, rport,
/// received) ride along, unread by a strategy that names its own fields.
pub fn via_cookie_params(via: &Via) -> RouteParams {
    via.params()
        .iter()
        .map(|(name, value)| {
            (name.as_str().to_ascii_lowercase(), value.as_str().unwrap_or_default().to_string())
        })
        .collect()
}

/// The call's owners a worker states on its own Via, `pri` and `bak`
/// (ADR-0055 §8): each empty when it states none. Read only off a worker's
/// request, on the internal face.
pub fn stated_owners(worker_via: &Via) -> StatedOwners {
    StatedOwners { primary: stated(worker_via, "pri"), backup: stated(worker_via, "bak") }
}

/// Whether a worker's Via states the call's primary: the Via of a leg's
/// dialog-creating request, or of the CANCEL and non-2xx ACK that share it.
pub fn states_primary(worker_via: &Via) -> bool {
    !stated(worker_via, "pri").is_empty()
}

fn stated(worker_via: &Via, name: &str) -> String {
    worker_via
        .param(name)
        .and_then(ParamValue::as_str)
        .map(sip_message::param_codec::decode_param)
        .unwrap_or_default()
}

/// The flag on a Via this proxy pushed on a request it forwarded internal to
/// internal: the response comes back on it (`crate::face`).
const SAME_FACE: &str = "same-face";

/// `via` with the marker of a hop that stayed on one face.
pub fn via_marking_same_face(via: Via) -> Via {
    via.with_param(SAME_FACE, ParamValue::Flag)
}

/// Whether `via`, one of this proxy's own, carries that marker.
pub fn via_marks_same_face(via: &Via) -> bool {
    via.param(SAME_FACE).is_some()
}

/// `via` carrying `cookie`'s fields, if any, after its own params; a field
/// named like a param `via` already carries is left out, never written over it.
/// A value that is no token is written as a quoted-string: a URI parameter's
/// `paramchar` is wider than a Via `gen-value` holds (RFC 3261 §25.1).
pub fn via_with_cookie(via: Via, cookie: Option<&RouteParams>) -> Via {
    cookie.into_iter().flatten().fold(via, |via, (name, value)| match via.param(name) {
        Some(_) => via,
        None => via.with_param(name.as_str(), via_value(value)),
    })
}

/// A cookie field's value as a Via `gen-value`: the token as written, any
/// other text quoted.
fn via_value(value: &str) -> ParamValue {
    if is_run_of(value, is_token_char, false) {
        ParamValue::Token(SipStr::owned(value))
    } else {
        ParamValue::Quoted(SipStr::owned(value))
    }
}

/// The transport destination a Route / Record-Route entry names, its `maddr`
/// where it has one ([`Uri::next_hop`]). The port defaults to 5060 (RFC 3261
/// §19.1.2); an out-of-range one never reaches here, because a URI that states
/// it does not parse.
pub fn route_target(uri: &Uri) -> ProxyAddr {
    let (host, port) = uri.next_hop();
    ProxyAddr::new(host, port)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sip_message::header::HeaderValue;
    use sip_message::sip_str::SipStr;

    use super::*;

    fn cookie() -> BTreeMap<String, String> {
        [("v".to_string(), "3".to_string()), ("w_pri".to_string(), "b2b-1".to_string())]
            .into_iter()
            .collect()
    }

    #[test]
    fn record_route_orders_params_then_lr() {
        let adv = ProxyAddr::new("10.0.0.1", 5060);
        assert_eq!(
            record_route(&adv, cookie().iter()).to_wire(),
            "<sip:10.0.0.1:5060;v=3;w_pri=b2b-1;lr>"
        );
    }

    #[test]
    fn a_flagged_record_route_marks_the_direction_before_lr() {
        let adv = ProxyAddr::new("10.0.0.1", 5060);
        assert_eq!(
            record_route_flagged(&adv, cookie().iter(), "outbound").to_wire(),
            "<sip:10.0.0.1:5060;v=3;w_pri=b2b-1;outbound;lr>"
        );
    }

    #[test]
    fn the_cookie_reads_back_off_our_own_entry() {
        let adv = ProxyAddr::new("10.0.0.1", 5060);
        let written = record_route(&adv, cookie().iter());
        let read_back = cookie_params(written.uri());
        assert_eq!(read_back.get("v").map(String::as_str), Some("3"));
        assert_eq!(read_back.get("w_pri").map(String::as_str), Some("b2b-1"));
        // The routing flag rides the same URI, so the cookie carries it too —
        // the strategy names the fields it decodes.
        assert!(read_back.contains_key("lr"));
        assert_eq!(route_target(written.uri()), adv);
    }

    #[test]
    fn a_cookie_field_never_replaces_a_param_the_via_carries() {
        let via = Via::udp("10.0.0.1", 5060).with_branch("z9hG4bKours").requesting_rport();
        let mut cookie = cookie();
        cookie.insert("branch".to_string(), "z9hG4bKforced".to_string());
        let stamped = via_with_cookie(via, Some(&cookie));
        assert_eq!(stamped.branch(), Some("z9hG4bKours"));
        assert_eq!(via_cookie_params(&stamped).get("w_pri").map(String::as_str), Some("b2b-1"));
    }

    /// A key id is any run of URI-parameter characters; on the Via, a value
    /// that is no token is quoted, and reads back as written.
    #[test]
    fn a_cookie_value_that_is_no_token_is_quoted_on_the_via() {
        let via = Via::udp("10.0.0.1", 5060).with_branch("z9hG4bKours");
        let mut cookie = cookie();
        cookie.insert("kid".to_string(), "k-1_2.v[3]:$".to_string());
        let wire = via_with_cookie(via, Some(&cookie)).to_wire();
        assert!(wire.contains(";kid=\"k-1_2.v[3]:$\""), "{wire}");
        assert!(wire.contains(";w_pri=b2b-1"), "a token stays as written: {wire}");
        let read = <Via as HeaderValue>::parse(&SipStr::owned(&wire)).expect("the Via reads");
        assert_eq!(via_cookie_params(&read).get("kid").map(String::as_str), Some("k-1_2.v[3]:$"));
    }

    #[test]
    fn a_flag_param_reads_back_as_a_present_key() {
        let uri = Uri::parse(&SipStr::owned("sip:10.0.0.1:5060;outbound;lr")).expect("parses");
        let params = cookie_params(&uri);
        assert!(params.contains_key("outbound"));
        assert_eq!(params.get("outbound").map(String::as_str), Some(""));
    }
}
