//! The egress routing policy for worker-originated requests: loose-route wire
//! destinations and the b-leg front-proxy bootstrap (RFC 3261 §16.12). The
//! deployment invariant: every B2BUA→callee message traverses the front proxy,
//! never pod-direct.

use sip_message::header::{HeaderValue, RouteEntry, Uri};
use sip_message::{SipRequest, SipStr};

use crate::config::B2buaConfig;

/// Apply the egress routing policy to an outbound in-dialog request.
///
/// Two effects, both RFC 3261 §16.12:
///   1. **Loose-route wire destination** (any leg): when the dialog's route set
///      is non-empty and its first route is a loose router (`;lr`), the request
///      is *sent* to that route's host:port while the Request-URI stays at the
///      remote target. The generator already emitted the Route headers from the
///      route set; this fixes only the wire destination so in-dialog requests
///      toward a record-routing proxy traverse it instead of going pod-direct.
///   2. **b-leg outbound-proxy bootstrap**: when the route set is empty (the
///      pre-confirmation initial INVITE), `leg_id` is a b-leg, and
///      `config.b2b_outbound_proxy` is set, preload a *plain* loose `Route` at the
///      proxy and redirect the wire destination there (the b-leg invariant: every
///      B2BUA→callee message traverses the front proxy). The proxy classifies the
///      initial INVITE worker-outbound from the top Via and double-record-routes
///      the dialog, so in-dialog direction is carried by the proxy's own
///      Record-Route thereafter — the worker stamps no `;outbound` (`ProxyCore`
///      §16.4 / §16.12).
///
/// `route_set` is the source dialog's route set in dialog order. For the a-leg
/// the natural route set (from the inbound INVITE's Record-Route) carries the
/// routing; `b2b_outbound_proxy` is a b-leg concept and is not applied there.
pub fn apply_b_leg_egress(
    config: &B2buaConfig,
    leg_id: &str,
    route_set: &[String],
    req: SipRequest,
    dest: (String, u16),
) -> (SipRequest, (String, u16)) {
    // (1) Loose-route: send to the top route's host:port (R-URI unchanged).
    if let Some(first) = route_set.first() {
        if let Some(uri) = loose_route_uri(first) {
            // The worker forwards the route set verbatim and stamps nothing:
            // under double-record-routing, the worker-facing half of the dialog
            // route set — captured from the dialog-creating message
            // (§12.1.1/§12.1.2) — is the proxy's own `;outbound` Record-Route on
            // top, and the proxy reads direction from its self-issued RR
            // (registry- and pod-IP-independent, so it survives a worker
            // reboot), not from anything the worker adds.
            let (host, port) = uri.host_port();
            return (req, (host.to_string(), port));
        }
        // Strict routing is handled by the generator's R-URI rewrite; the wire
        // destination already resolves to the first route via `remote_target`.
        return (req, dest);
    }
    // (2) Empty route set (pre-confirmation INVITE) + b-leg outbound-proxy
    // bootstrap. There is no dialog route set yet, so preload a plain loose Route
    // to the front proxy to get the initial INVITE there; the proxy classifies it
    // worker-outbound from the top Via (the originating worker is live and
    // registered at call set-up — the reboot window only affects in-dialog traffic
    // of EXISTING calls, which the double-record-route above covers) and double-
    // record-routes the dialog so every subsequent in-dialog request is direction-
    // correct without a worker-stamped marker.
    if leg_id == "a" {
        return (req, dest);
    }
    let Some((route, (host, port))) = outbound_proxy_route(config) else {
        return (req, dest);
    };
    match req.thaw().prepend(route).freeze() {
        Ok(preloaded) => (preloaded, (host, port)),
        // The preload failed, so the request carries no Route naming the proxy —
        // but its Request-URI already names the callee, so the proxy forwards it
        // and record-routes the dialog. Sending it to `dest` instead would put
        // this leg pod-direct, which the deployment forbids (every worker-
        // originated request traverses the front proxy).
        Err(err) => {
            tracing::warn!(
                %leg_id,
                error = %err,
                %host,
                port,
                "b-leg egress could not preload the outbound-proxy Route; forwarding WITHOUT it \
                 rather than pod-direct"
            );
            (req, (host, port))
        }
    }
}

/// The plain loose `Route` naming the configured front proxy, with the wire
/// destination it resolves to. `None` when no outbound proxy is configured
/// (local/dev, where the transport IS peer-direct).
fn outbound_proxy_route(config: &B2buaConfig) -> Option<(RouteEntry, (String, u16))> {
    let (host, port) = config.b2b_outbound_proxy.clone()?;
    let route = RouteEntry::from_uri(Uri::sip(host.clone()).with_port(port).with_flag("lr"));
    Some((route, (host, port)))
}

/// The dialog route set a call falls back to when the peer's recorded routes do
/// not read: the one loose `Route` at the configured front proxy — the same
/// entry [`apply_b_leg_egress`] preloads for a pre-confirmation b-leg — so
/// in-dialog requests keep traversing the proxy instead of going pod-direct.
/// Empty when no outbound proxy is configured.
pub fn outbound_proxy_route_set(config: &B2buaConfig) -> Vec<String> {
    outbound_proxy_route(config)
        .map(|(route, _)| route.to_wire())
        .into_iter()
        .collect()
}

/// The URI of `route` when it names a loose router (RFC 3261 §19.1.1 `;lr`).
fn loose_route_uri(route: &str) -> Option<Uri> {
    let entry = RouteEntry::parse(&SipStr::owned(route)).ok()?;
    entry.uri().is_loose_route().then(|| entry.uri().clone())
}

/// Egress-aware wire destination for a leg's in-dialog request, WITHOUT mutating
/// a request. Mirrors `apply_b_leg_egress`'s destination decision (keep in sync) —
/// used for observability attribution (the keepalive-timeout peer metric), where
/// we need the wire hop the unanswered OPTIONS used but must not synthesize a
/// request to find it.
pub fn leg_egress_dest(
    config: &B2buaConfig,
    leg_id: &str,
    route_set: &[String],
    base_dest: (String, u16),
) -> (String, u16) {
    if let Some(first) = route_set.first() {
        if let Some(uri) = loose_route_uri(first) {
            let (host, port) = uri.host_port();
            return (host.to_string(), port);
        }
        return base_dest;
    }
    if leg_id == "a" {
        return base_dest;
    }
    if let Some((host, port)) = config.b2b_outbound_proxy.clone() {
        return (host, port);
    }
    base_dest
}

#[cfg(test)]
mod egress_tests {
    use super::*;
    use sip_message::header::HeaderName;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    fn parse(raw: &str) -> SipRequest {
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// The one Route line the request carries.
    fn top_route(req: &SipRequest) -> String {
        req.raw(HeaderName::Route).next().expect("route header").to_string()
    }

    fn in_dialog_options(route: &str) -> SipRequest {
        parse(&format!(
            "OPTIONS sip:sipp@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.244.1.5:5060;branch=z9hG4bKa;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:sipp@10.244.2.7:5060>;tag=uac\r\n\
Call-ID: c1@x\r\n\
CSeq: 2 OPTIONS\r\n\
Route: {route}\r\n\
Content-Length: 0\r\n\r\n"
        ))
    }

    // A worker-originated in-dialog request loose-routes back through our front
    // proxy on the route set captured at dialog set-up. The worker stamps
    // nothing: under double-record-routing the worker-facing half of that route
    // set is ALREADY the proxy's own `;outbound` Record-Route, so egress just
    // forwards the top Route verbatim and resolves the wire destination to it.
    // (The proxy reads direction from its own self-issued RR — registry- and
    // pod-IP-independent, so it survives a worker reboot.)
    #[test]
    fn worker_in_dialog_loose_route_is_forwarded_verbatim() {
        let route = "<sip:10.0.0.9:5060;outbound;lr>";
        let (out, dest) = apply_b_leg_egress(
            &B2buaConfig::default(),
            "a",
            &[route.to_string()],
            in_dialog_options(route),
            ("10.244.2.7".to_string(), 5060),
        );
        // The top Route is unchanged (the proxy issued the `;outbound`, not us).
        assert_eq!(top_route(&out), route, "egress must forward the captured route set verbatim");
        // Loose route → wire destination is the proxy (top route); R-URI unchanged.
        assert_eq!(dest, ("10.0.0.9".to_string(), 5060));
    }

    // The worker does NOT add `;outbound` to a cookie route it did not issue: a
    // route set whose top is the proxy's stickiness cookie (no `;outbound`) is
    // forwarded untouched (this is the EXTERNAL-facing half — it should never be
    // on top of a worker-originated request, but egress must not mutate it).
    #[test]
    fn worker_in_dialog_does_not_stamp_outbound() {
        let route = "<sip:10.0.0.9:5060;target=10.244.1.5:5060;lr>";
        let (out, dest) = apply_b_leg_egress(
            &B2buaConfig::default(),
            "a",
            &[route.to_string()],
            in_dialog_options(route),
            ("10.244.2.7".to_string(), 5060),
        );
        let forwarded = top_route(&out);
        let uri = loose_route_uri(&forwarded).expect("a loose route");
        assert!(
            uri.param("outbound").is_none(),
            "egress must not stamp ;outbound; got {forwarded}"
        );
        assert_eq!(dest, ("10.0.0.9".to_string(), 5060));
    }

    // The pre-confirmation b-leg INVITE (empty route set) preloads a PLAIN loose
    // Route to the outbound proxy — no `;outbound`. The proxy classifies the
    // initial INVITE worker-outbound from the top Via (the originating worker is
    // live at set-up) and double-record-routes the dialog from there.
    #[test]
    fn b_leg_bootstrap_preloads_plain_loose_route() {
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));
        let invite = parse(
            "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.244.1.5:5060;branch=z9hG4bKb;lg=b\r\n\
Max-Forwards: 70\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Call-ID: c2@x\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n",
        );
        let (out, dest) = apply_b_leg_egress(&config, "b-1", &[], invite, ("10.244.2.7".to_string(), 5060));
        let preloaded = top_route(&out);
        assert_eq!(preloaded, "<sip:10.0.0.9:5060;lr>", "bootstrap preload must be a plain loose Route");
        let uri = loose_route_uri(&preloaded).expect("a loose route");
        assert!(uri.param("outbound").is_none(), "no ;outbound on the bootstrap preload");
        assert_eq!(dest, ("10.0.0.9".to_string(), 5060), "wire destination is the outbound proxy");
    }

    // `leg_egress_dest` mirrors apply_b_leg_egress's destination decision WITHOUT
    // mutating a request — used for keepalive-timeout peer attribution. It must
    // agree with the BYE path on every branch: loose-route → top route host; empty
    // route-set b-leg + outbound proxy → the proxy; a-leg / no proxy → base.
    #[test]
    fn leg_egress_dest_mirrors_apply_b_leg_egress() {
        let base = ("10.244.2.7".to_string(), 5060);

        // (1) Loose route on top → wire dest is the top route's host:port.
        let route = "<sip:10.0.0.9:5060;outbound;lr>".to_string();
        assert_eq!(
            leg_egress_dest(&B2buaConfig::default(), "b-1", &[route.clone()], base.clone()),
            ("10.0.0.9".to_string(), 5060),
            "loose route → top route host:port",
        );
        // Agrees with the request-mutating path's destination.
        let (_, mut_dest) = apply_b_leg_egress(
            &B2buaConfig::default(),
            "b-1",
            &[route],
            in_dialog_options("<sip:10.0.0.9:5060;outbound;lr>"),
            base.clone(),
        );
        assert_eq!(mut_dest, ("10.0.0.9".to_string(), 5060));

        // (2) Empty route set, b-leg, outbound proxy configured → the proxy.
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));
        assert_eq!(
            leg_egress_dest(&config, "b-1", &[], base.clone()),
            ("10.0.0.9".to_string(), 5060),
            "empty route-set b-leg + outbound proxy → the proxy",
        );

        // (3) a-leg with empty route set → the base remote target (the proxy
        // bootstrap is a b-leg-only concept).
        assert_eq!(
            leg_egress_dest(&config, "a", &[], base.clone()),
            base.clone(),
            "a-leg → base remote target (no proxy bootstrap on the a-leg)",
        );

        // (4) b-leg, empty route set, NO outbound proxy → the base.
        assert_eq!(
            leg_egress_dest(&B2buaConfig::default(), "b-1", &[], base.clone()),
            base,
            "no outbound proxy → base remote target",
        );
    }
}
