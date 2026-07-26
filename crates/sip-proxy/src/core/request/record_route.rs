//! Double-Record-Route insertion — in-dialog DIRECTION is intrinsic to the
//! proxy's own self-issued RR pair (stickiness-cookie half + `;outbound`
//! half), each half stamped with the advertise of the face that will use it.
//! Route *popping* / direction *reading* does NOT live here — see
//! [`route`](super::route).

use std::net::SocketAddr;

use sip_message::draft::RequestDraft;
use sip_message::{SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::headers::{record_route, record_route_flagged};

use super::super::{is_dialog_creating, ProxyCore};

impl ProxyCore {
    /// Insert the double Record-Route on an INITIAL dialog-creating request
    /// (no To-tag); a no-op otherwise. A mid-dialog re-INVITE / target-refresh
    /// (To-tag present) reuses the route set already fixed at dialog creation
    /// (RFC 3261 §12.2), so re-inserting RR is inert bloat and never alters
    /// the established route set.
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
    ) -> RequestDraft {
        let method = req.method.as_str();
        let is_initial_dialog_req = req.to().tag().map(str::is_empty).unwrap_or(true);
        if !is_dialog_creating(method) || !is_initial_dialog_req {
            return draft;
        }
        // Double record-route so in-dialog DIRECTION is intrinsic to the
        // proxy's own Record-Route — no worker-stamped `;outbound`. We insert
        // two RRs:
        //   • cookie RR  — used by the EXTERNAL party to reach the worker
        //     (decode → w_pri, registry-keyed, so it survives a worker pod-IP
        //     change after reboot).
        //   • outbound RR — used by the WORKER to reach the external party
        //     (classified `;outbound` → forward to the R-URI).
        // The §12.1.1 (UAS, forward) / §12.1.2 (UAC, reverse) route-set rule
        // then puts the right half on top of each party's route set on its
        // own; here we only choose which faces the *next hop*: forwarding TO a
        // worker (inbound) puts the outbound/worker-facing RR on top, forwarding
        // to the external party (worker-outbound) puts the cookie RR on top.
        // `push_front` puts a line at the top of the message (§16.6 places the
        // proxy's own headers there), so push the lower half first.
        // For a worker-originated request the cookie identifies the
        // ORIGINATING worker, taken from the SNAT-immune Via identity —
        // NOT the UDP source: behind the keepalived VIP the source is the
        // node IP + an ephemeral port, which matches no registry entry, so
        // encoding for the source silently produces a param-less cookie RR and
        // the callee's later in-dialog requests decode Unknown and are
        // re-sharded to an arbitrary worker (the b-leg variant of the
        // long-call-loss class). `src` stays as the pod-direct fallback.
        let cookie_addr = if is_worker_outbound {
            via_worker_addr.clone().unwrap_or_else(|| ProxyAddr::from(src))
        } else {
            target.clone()
        };
        let stickiness = self.strategy.encode_stickiness(&cookie_addr, msg);
        // Dual-face (§16.6/§16.7 multi-homed): stamp each half of the
        // double-RR with the advertise of the face facing the party that
        // will USE it —
        //   • cookie RR   → the EXTERNAL party's face (the caller for an
        //     inbound dialog-forming request = the source's face; the
        //     callee for a worker-outbound one = the target's face), so
        //     that party's in-dialog requests reach the proxy on the face
        //     it lives on;
        //   • outbound RR → the WORKER party's face (the forward target
        //     for inbound; the originating worker for worker-outbound) —
        //     the internal plane in the deployed topology.
        // The §12.1.1/§12.1.2 route-set rules then put the correct face on
        // top of each party's route set on their own. When both resolve to
        // the SAME face (single-face mode, intra-face forwarding) this is
        // byte-identical to the single-advertise double-RR.
        let external_party_adv = if is_worker_outbound {
            self.egress_advertised(target)
        } else {
            self.advertised_for_ip(src.ip())
        };
        let worker_party_adv = if is_worker_outbound {
            self.egress_advertised(&cookie_addr)
        } else {
            self.egress_advertised(target)
        };
        let cross_face = external_party_adv != worker_party_adv;
        let cookie_rr = match &stickiness {
            Some(params) => record_route(external_party_adv, params.iter()),
            None => record_route(external_party_adv, std::iter::empty()),
        };
        // Cross-face, the stickiness cookie rides BOTH entries: whichever
        // self-RR a response's reverse-failover (or a diagnostic) recovers
        // params from, the signed worker pin is present. Intra-face keeps
        // the param-less `;outbound;lr` form.
        let outbound_rr = match (&stickiness, cross_face) {
            (Some(params), true) => record_route_flagged(worker_party_adv, params.iter(), "outbound"),
            _ => record_route_flagged(worker_party_adv, std::iter::empty(), "outbound"),
        };
        let draft = if is_worker_outbound {
            draft.push_front(outbound_rr).push_front(cookie_rr)
        } else {
            draft.push_front(cookie_rr).push_front(outbound_rr)
        };
        self.metrics.record_route_inserted();
        draft
    }
}
