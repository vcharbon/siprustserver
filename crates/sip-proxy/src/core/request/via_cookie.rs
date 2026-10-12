//! The stickiness cookie on the Via this proxy pushes on a worker's in-dialog
//! INVITE, and on that INVITE's CANCEL and ACK. A response bound for a worker
//! comes back along the Via (RFC 3261 §18.2.2); one answering an in-dialog
//! request carries no Record-Route (§12.1.1), so when that worker has died
//! reverse-path failover (`core/response`) reads the cookie off this Via in its
//! place. The cookie is the dialog's own, as the strategy signed it, read off
//! the request's route set: the INVITE, its CANCEL (§9.1) and its ACK
//! (§17.1.1.3) carry the same Route, so they carry the same Via at any proxy
//! instance, remembering nothing.

use sip_message::header::{RouteEntry, Uri};
use sip_message::{Method, SipRequest};

use crate::headers::cookie_params;
use crate::strategy::RouteParams;

use super::super::ProxyCore;

/// The routing flags this proxy puts on its own Route / Record-Route URIs:
/// beside the cookie, never part of it.
const ROUTING_FLAGS: [&str; 2] = ["lr", "outbound"];

impl ProxyCore {
    /// The cookie for the Via pushed on `req`, a worker-outbound request: for
    /// an INVITE, a CANCEL or an ACK, the cookie one of `own_routes` (the
    /// proxy's entries the request's route set carried) names a primary in.
    /// `None` for any other request, and for one whose route set holds no
    /// cookie (an initial INVITE's preloaded Route).
    pub(super) fn via_cookie(
        &self,
        req: &SipRequest,
        own_routes: &[RouteEntry],
    ) -> Option<RouteParams> {
        if !matches!(*req.method(), Method::Invite | Method::Cancel | Method::Ack) {
            return None;
        }
        own_routes
            .iter()
            .map(|route| cookie_of(route.uri()))
            .find(|cookie| self.strategy.stickiness_primary(cookie).is_some())
    }
}

/// The cookie on one of the proxy's own Route URIs, its routing flags left out.
fn cookie_of(uri: &Uri) -> RouteParams {
    let mut cookie = cookie_params(uri);
    for flag in ROUTING_FLAGS {
        cookie.remove(flag);
    }
    cookie
}
