//! `StrictRouteRewriteHandled` on a URI's `maddr`: a proxy's own Route is its by
//! authority or next hop, and a Request-URI is addressed to it by authority alone.

use crate::verdict::{Decision, Evidence};

use super::{routed, strict, ALICE, PROXY};

/// A loose entry naming the proxy by its authority is popped (§16.4) whatever its
/// `maddr` says, and the strict entry after it is owed the Request-URI.
#[test]
fn a_loose_route_naming_the_proxy_by_authority_is_popped_whatever_its_maddr() {
    let route = format!("Route: <sip:{PROXY};maddr=192.0.2.9;lr>, <sip:strict@h>\r\n");
    let msgs = [routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", &route)];
    let f = strict(&msgs);
    assert_eq!(f.len(), 1, "the strict entry past the proxy's own: {f:?}");
    let Decision::Violated(Evidence::StrictRouteNotRewritten { first_route, .. }) = &f[0].decision
    else {
        panic!("strict-route evidence: {:?}", f[0].decision)
    };
    assert_eq!(first_route, "sip:strict@h");
}

/// A Request-URI naming the proxy only by its `maddr` is §16.4's strip, not a
/// strict-routing peer addressing the proxy: a loose first Route stays no occasion.
#[test]
fn a_request_uri_naming_the_proxy_by_its_maddr_is_not_addressed_to_it() {
    let (host, port) = PROXY.split_once(':').expect("host:port");
    let uri = format!("sip:x@foreign.example:{port};maddr={host}");
    let route = "Route: <sip:a@h;lr>, <sip:strict@h>\r\n";
    let msgs = [routed(1_000, ALICE, PROXY, "z9hG4bK-in", &uri, route)];
    assert!(strict(&msgs).is_empty(), "{:?}", strict(&msgs));
}
