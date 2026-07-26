//! The proxy's Record-Route policy: the entries this proxy records on a dialog
//! and the stickiness cookie read back off one.
//!
//! Header *mechanics* — prepending a hop, popping an entry, stamping
//! received/rport — are sip-message draft vocabulary (ADR-0025). What rides the
//! proxy's OWN Record-Route is policy, and policy is what lives here.

use sip_message::header::{ParamValue, RecordRouteEntry, Uri};

use crate::addr::ProxyAddr;
use crate::strategy::RouteParams;

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
        uri = uri.with_param(name.as_str(), ParamValue::text(value.as_str()));
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

/// The transport destination a Route / Record-Route entry names. The port
/// defaults to 5060 (RFC 3261 §19.1.2); an out-of-range one never reaches here,
/// because a URI that states it does not parse.
pub fn route_target(uri: &Uri) -> ProxyAddr {
    let (host, port) = uri.host_port();
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
    fn a_flag_param_reads_back_as_a_present_key() {
        let uri = Uri::parse(&SipStr::owned("sip:10.0.0.1:5060;outbound;lr")).expect("parses");
        let params = cookie_params(&uri);
        assert!(params.contains_key("outbound"));
        assert_eq!(params.get("outbound").map(String::as_str), Some(""));
    }
}
