//! Double-Record-Route insertion — in-dialog DIRECTION is intrinsic to the
//! proxy's own self-issued RR pair (stickiness-cookie half + `;outbound`
//! half), the top entry stamped with the egress face's advertise and the lower
//! one with the ingress face's.
//! Route *popping* / direction *reading* does NOT live here — see
//! [`route`](super::route).

use std::net::SocketAddr;

use sip_message::draft::RequestDraft;
use sip_message::{SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::headers::{record_route, record_route_flagged, stated_owners};
use crate::observability::metrics::Face;
use crate::strategy::RouteParams;

use super::super::{is_dialog_creating, ProxyCore};

impl ProxyCore {
    /// Insert the double Record-Route on an initial dialog-creating request
    /// (no To-tag); a no-op otherwise, since a mid-dialog request reuses the
    /// route set fixed at dialog creation (RFC 3261 §12.2). Returns the draft
    /// and the stickiness cookie the inserted entries carry (`None` when nothing
    /// was inserted or the strategy has no per-dialog stickiness). `repeated` is
    /// the cookie the first copy of a retransmitted request carried: it is used
    /// again. `faces` is the request's ingress and egress face.
    /// The cookie of a worker's dialog names the owners the worker states for
    /// its call (ADR-0055 §8); the cookie of a dialog the proxy places names the
    /// backup the strategy picks.
    /// Cross-face the cookie rides both entries, so a reverse failover finds it
    /// on whichever of this proxy's entries it reads; within one face the
    /// `;outbound` entry carries none.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn insert_double_record_route(
        &self,
        draft: RequestDraft,
        msg: &SipMessage,
        req: &SipRequest,
        src: SocketAddr,
        target: &ProxyAddr,
        is_worker_outbound: bool,
        via_worker_addr: &Option<ProxyAddr>,
        repeated: Option<&RouteParams>,
        faces: (Face, Face),
    ) -> (RequestDraft, Option<RouteParams>) {
        let method = req.method().as_str();
        let is_initial_dialog_req = req.to().tag().map(str::is_empty).unwrap_or(true);
        if !is_dialog_creating(method) || !is_initial_dialog_req {
            return (draft, None);
        }
        let cookie_addr = if is_worker_outbound {
            via_worker_addr.clone().unwrap_or_else(|| ProxyAddr::from(src))
        } else {
            target.clone()
        };
        let stickiness = repeated.cloned().or_else(|| {
            if is_worker_outbound {
                let stated = stated_owners(req.top_via());
                self.strategy.encode_originated(&cookie_addr, msg, &stated)
            } else {
                self.strategy.encode_stickiness(&cookie_addr, msg)
            }
        });
        let (ingress, egress) = faces;
        let (ingress_adv, egress_adv) = (self.advertised_of(ingress), self.advertised_of(egress));
        let (external_party_adv, worker_party_adv) =
            if is_worker_outbound { (egress_adv, ingress_adv) } else { (ingress_adv, egress_adv) };
        let cross_face = ingress_adv != egress_adv;
        let cookie_rr = match &stickiness {
            Some(params) => record_route(external_party_adv, params.iter()),
            None => record_route(external_party_adv, std::iter::empty()),
        };
        let outbound_rr = match (&stickiness, cross_face) {
            (Some(params), true) => {
                record_route_flagged(worker_party_adv, params.iter(), "outbound")
            }
            _ => record_route_flagged(worker_party_adv, std::iter::empty(), "outbound"),
        };
        let draft = if is_worker_outbound {
            draft.push_front(outbound_rr).push_front(cookie_rr)
        } else {
            draft.push_front(cookie_rr).push_front(outbound_rr)
        };
        self.metrics.record_route_inserted();
        (draft, stickiness)
    }
}
