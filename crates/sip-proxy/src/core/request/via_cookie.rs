//! The stickiness cookie on the Via this proxy pushes on a worker's INVITE,
//! and on that INVITE's CANCEL and ACK. A response bound for a worker comes
//! back along the Via (RFC 3261 §18.2.2), so reverse-path failover
//! (`core/response`) reads off this Via the cookie naming the worker the
//! request came from: in place of the Record-Route an in-dialog request's
//! answer lacks (this proxy never Record-Routes an in-dialog request, and a
//! callee's answer carries the Record-Route only of a dialog-creating request,
//! §12.1.1), and after that worker's address has stopped naming it.
//!
//! The cookie is the dialog's own, read off the request's route set. A request
//! whose route set holds none — an initial INVITE's preloaded Route, its
//! CANCEL and its non-2xx ACK — carries the cookie minted for its originating
//! worker, the one an initial INVITE's Record-Route is minted with. The
//! INVITE, its CANCEL (§9.1) and its ACK (§17.1.1.3) carry the same Route and
//! name the same worker, so they carry the same Via at any proxy instance,
//! remembering nothing. Only the strategy's own fields ride, and only once
//! they verify for the call (`RoutingStrategy::verified_cookie`): a strategy
//! whose cookie names no worker puts none on the Via.

use sip_message::header::RouteEntry;
use sip_message::{Method, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::headers::cookie_params;
use crate::strategy::RouteParams;

use super::super::ProxyCore;

impl ProxyCore {
    /// The cookie for the Via pushed on `req` (the request `msg` holds), a
    /// worker-outbound request from the worker at `originator`: for an
    /// INVITE, a CANCEL or an ACK, the first verified cookie of `own_routes`
    /// (the proxy's entries the request's route set carried), else the cookie
    /// minted for `originator`. `None` for any other request.
    pub(super) fn via_cookie(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        own_routes: &[RouteEntry],
        originator: &ProxyAddr,
    ) -> Option<RouteParams> {
        if !matches!(*req.method(), Method::Invite | Method::Cancel | Method::Ack) {
            return None;
        }
        own_routes
            .iter()
            .find_map(|route| self.strategy.verified_cookie(&cookie_params(route.uri()), msg))
            .or_else(|| {
                let minted = self.strategy.encode_stickiness(originator, msg)?;
                self.strategy.verified_cookie(&minted, msg)
            })
    }
}
