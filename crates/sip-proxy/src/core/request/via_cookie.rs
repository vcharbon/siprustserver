//! The stickiness cookie on the Via this proxy pushes on a worker's in-dialog
//! INVITE, and on that INVITE's CANCEL and ACK. A response bound for a worker
//! comes back along the Via (RFC 3261 §18.2.2); the answer to an in-dialog
//! request carries no Record-Route of this proxy's — it never Record-Routes an
//! in-dialog request (`record_route`), and a callee's answer carries the
//! Record-Route only of a dialog-creating request (§12.1.1) — so when that
//! worker has died reverse-path failover (`core/response`) reads the cookie off
//! this Via in its place. The cookie is the dialog's own,
//! read off the request's route set: the INVITE, its CANCEL (§9.1) and its ACK
//! (§17.1.1.3) carry the same Route, so they carry the same Via at any proxy
//! instance, remembering nothing. Only the strategy's own fields ride, and only
//! once they verify for the call (`RoutingStrategy::verified_cookie`).

use sip_message::header::RouteEntry;
use sip_message::{Method, SipMessage, SipRequest};

use crate::headers::cookie_params;
use crate::strategy::RouteParams;

use super::super::ProxyCore;

impl ProxyCore {
    /// The cookie for the Via pushed on `req` (the request `msg` holds), a
    /// worker-outbound request: for an INVITE, a CANCEL or an ACK, the first
    /// verified cookie of `own_routes` (the proxy's entries the request's route
    /// set carried). `None` for any other request, and for one whose route set
    /// holds no verified cookie (an initial INVITE's preloaded Route).
    pub(super) fn via_cookie(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        own_routes: &[RouteEntry],
    ) -> Option<RouteParams> {
        if !matches!(*req.method(), Method::Invite | Method::Cancel | Method::Ack) {
            return None;
        }
        own_routes
            .iter()
            .find_map(|route| self.strategy.verified_cookie(&cookie_params(route.uri()), msg))
    }
}
