//! The stickiness cookie on the Via this proxy pushes on a worker's INVITE,
//! and on that INVITE's CANCEL and ACK. A response bound for a worker comes
//! back along the Via (RFC 3261 §18.2.2), so reverse-path failover
//! (`core/response`) reads off this Via the cookie naming the worker the
//! request came from: in place of the Record-Route an in-dialog request's
//! answer lacks (this proxy never Record-Routes an in-dialog request, and a
//! callee's answer carries the Record-Route only of a dialog-creating request,
//! §12.1.1), and after that worker's address has stopped naming it.
//!
//! An INVITE's cookie is the dialog's own, read off the request's route set;
//! a request whose route set holds none — an initial INVITE's preloaded Route
//! — carries the cookie its Record-Route is minted with. A CANCEL (§9.1) and a
//! non-2xx ACK (§17.1.1.3) carry their INVITE's Via, cookie included: the one
//! the INVITE's entry remembers, whatever the worker set did since, whatever
//! the INVITE's own headers said that theirs do not (an emergency
//! Resource-Priority), and after the worker's address stopped naming it. With
//! no entry they carry the cookie the INVITE would have carried. Only the
//! strategy's own fields ride, and only once they verify for the call
//! (`RoutingStrategy::verified_cookie`): a strategy whose cookie names no
//! worker puts none on the Via.

use sip_message::{Method, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::cancel_lru::invite_txn_key;
use crate::headers::cookie_params;
use crate::strategy::RouteParams;

use super::super::ProxyCore;
use super::classify::Classified;

impl ProxyCore {
    /// The cookie for the Via pushed on `req` (the request `msg` holds),
    /// classified as `c`, from the worker at `originator`; `minted` is the
    /// cookie its Record-Route carries, if any. For a CANCEL or an ACK, the
    /// cookie its INVITE's entry remembers. Else, for a worker-outbound INVITE,
    /// CANCEL or ACK, the first verified cookie of the proxy's own route
    /// entries, else `minted`, else the cookie minted for `originator`. `None`
    /// for any other request.
    pub(super) fn via_cookie(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        c: &Classified,
        originator: &ProxyAddr,
        minted: Option<&RouteParams>,
    ) -> Option<RouteParams> {
        let method = req.method();
        if !matches!(*method, Method::Invite | Method::Cancel | Method::Ack) {
            return None;
        }
        if matches!(*method, Method::Cancel | Method::Ack) {
            let call_id = req.call_id();
            let from = req.from();
            let key = invite_txn_key(req.top_via(), call_id.as_str(), from.tag(), req.cseq().seq());
            if let Some(invite) = self.cancel_lru.lookup(&key) {
                return invite.via_cookie;
            }
        }
        if !c.worker_outbound {
            return None;
        }
        c.own_routes
            .iter()
            .find_map(|route| self.strategy.verified_cookie(&cookie_params(route.uri()), msg))
            .or_else(|| {
                let own =
                    minted.cloned().or_else(|| self.strategy.encode_stickiness(originator, msg))?;
                self.strategy.verified_cookie(&own, msg)
            })
    }
}
