//! What a forward leaves in the branch memo: only what no later request of
//! its transaction can recompute (ADR-0009 X7).
//!
//! - An INVITE's entry, for its CANCEL and its non-2xx ACK, only when their
//!   memo-free route may miss the node the INVITE went to: a placement off the
//!   rendezvous winner among every worker the pool knows (a spill, a
//!   band-filtered winner, a winner refusing new calls or read not alive,
//!   whose return would draw the CANCEL back), or on a cookie's backup while
//!   its primary reads not alive; counted as a deviation. A worker's
//!   INVITE goes to its loose Route or Request-URI and a cookie-routed one
//!   where its cookie places it, which their CANCEL and ACK repeat; a worker's
//!   INVITE keeps its entry only when its CANCEL could not rebuild its Via:
//!   its sender no longer reads alive (the address the CANCEL is recognised
//!   by lapses), or it states an emergency (a cookie flag only the INVITE's
//!   Resource-Priority carries).
//! - A retransmission entry only for a request whose first copy met a fresh
//!   selection or was counted as a new call: a copy must neither meet the
//!   gate and the caps again nor be counted again, and its target depends on
//!   the load the first copy met. A request routed by its Route, its
//!   Request-URI or its cookie is routed the same way again.

use sip_message::{Method, SipMessage};

use crate::addr::ProxyAddr;
use crate::observability::metrics::RoutingDecisionKind;

use super::super::ProxyCore;
use super::classify::Classified;

/// How a forward's target was found, as far as the memo cares.
pub(super) struct Placement<'a> {
    pub(super) msg: &'a SipMessage,
    pub(super) classified: &'a Classified,
    pub(super) decision: RoutingDecisionKind,
    /// The cookie's verdict when a cookie was read (`hit`, `backup`, `miss`).
    pub(super) verdict: Option<&'static str>,
    /// Where the request's first copy went.
    pub(super) went: &'a ProxyAddr,
    /// The worker that sent it, by its Via or its source.
    pub(super) originator: &'a ProxyAddr,
    pub(super) emergency: bool,
}

impl Placement<'_> {
    /// The target came from a fresh selection: the gate, the caps and the
    /// load decided it.
    fn afresh(&self) -> bool {
        self.decision == RoutingDecisionKind::SelectNew || self.verdict == Some("miss")
    }
}

/// Why an INVITE's entry is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InviteEntry {
    /// Its CANCEL and ACK find their route without it.
    Recomputed,
    /// It went off its memo-free route.
    Deviation,
    /// A worker's INVITE whose Via its CANCEL could not rebuild.
    WorkerVia,
}

impl ProxyCore {
    /// Whether the INVITE placed as `p` keeps an entry, and why.
    pub(super) fn invite_entry(&self, p: &Placement<'_>) -> InviteEntry {
        let c = p.classified;
        if c.worker_outbound {
            let alive = self.registry.lookup_by_address(p.originator).is_some_and(|w| w.is_alive());
            return if alive && !p.emergency {
                InviteEntry::Recomputed
            } else {
                InviteEntry::WorkerVia
            };
        }
        if c.loose_route_next_hop.is_some() {
            return InviteEntry::Recomputed;
        }
        match p.verdict {
            // The cookie places the INVITE's CANCEL where it placed the INVITE
            // while that primary lives; once it is gone, on the backup holding
            // the call, which takes the INVITE's transaction over.
            Some("hit") => InviteEntry::Recomputed,
            // Placed on the backup while its primary reads not alive: the
            // primary's return would draw the CANCEL back to it.
            Some("backup") => InviteEntry::Deviation,
            _ => deviation_unless(self.strategy.holds_by_rank(p.msg, p.went)),
        }
    }
}

fn deviation_unless(on_route: bool) -> InviteEntry {
    if on_route {
        InviteEntry::Recomputed
    } else {
        InviteEntry::Deviation
    }
}

/// Whether the forward of a `method` request placed as `p` keeps a
/// retransmission entry: an initial INVITE (counted as a call, gated once),
/// a request placed afresh, or a copy of one that kept its entry.
pub(super) fn keeps_retransmission(
    method: &Method,
    initial_invite: bool,
    p: &Placement<'_>,
    repeats_entry: bool,
) -> bool {
    match method {
        Method::Cancel | Method::Ack => false,
        _ => initial_invite || repeats_entry || p.afresh(),
    }
}
