//! The stickiness cookie on the Via this proxy pushes on a worker's request,
//! whatever its method. A response bound for a worker comes back along the Via
//! (RFC 3261 §18.2.2), and the response path (`core/response`) reads the call's
//! owners off it (ADR-0055 §8) where the answer carries no Record-Route of this
//! proxy. A CANCEL (§9.1) and a non-2xx ACK (§17.1.1.3) carry their INVITE's
//! cookie, whatever the worker set did since. Only the strategy's own fields
//! ride, once they verify for the call. See ADR-0009 X1.

use sip_message::{Method, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::cancel_lru::invite_txn_key;
use crate::headers::{cookie_params, stated_owners, states_primary};
use crate::strategy::RouteParams;

use super::super::ProxyCore;
use super::classify::Classified;

impl ProxyCore {
    /// The cookie for the Via pushed on `req` (the request `msg` holds),
    /// classified as `c`, from the worker at `originator`; `minted` is the
    /// cookie its Record-Route carries, if any. For a CANCEL or an ACK, the
    /// cookie its INVITE's entry remembers. Else, for a worker-outbound
    /// request of any method, the first verified cookie of the proxy's own
    /// route entries, else `minted`, else the cookie minted for `originator`
    /// and the owners it states, with the strategy's signed dialling mark
    /// beside it when the top Via states the call's primary (a leg's dialling
    /// INVITE, its CANCEL and its non-2xx ACK, ADR-0055 §8). `None` for any
    /// other request.
    pub(super) fn via_cookie(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        c: &Classified,
        originator: &ProxyAddr,
        minted: Option<&RouteParams>,
    ) -> Option<RouteParams> {
        let method = req.method();
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
        let mut cookie = c
            .own_routes
            .iter()
            .find_map(|route| self.strategy.verified_cookie(&cookie_params(route.uri()), msg))
            .or_else(|| {
                let own = minted.cloned().or_else(|| {
                    let stated = stated_owners(req.top_via());
                    self.strategy.encode_originated(originator, msg, &stated)
                })?;
                self.strategy.verified_cookie(&own, msg)
            })?;
        if states_primary(req.top_via()) {
            cookie.extend(self.strategy.dialling_mark(&cookie));
        }
        Some(cookie)
    }
}
