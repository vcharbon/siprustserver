//! Which requests are new calls, and the proxy-self gate (intake-age/CPS
//! admission) a new call meets: a refused one is answered 503 with
//! Retry-After and no Reason, the same bytes for every copy; an emergency
//! call bypasses the gate.
//!
//! A new call is an initial INVITE (no To-tag), first transmission only, that
//! is not worker-outbound. On the external face an INVITE with a To-tag that
//! no verified cookie backs is a new call too, where it falls to a fresh
//! selection (the To-tag rule, [`ProxyCore::admits_as_new`], ADR-0048): it
//! meets the gate and the strategy's new-call caps, and is never refused for
//! its To-tag alone.

use std::net::SocketAddr;

use sip_message::{Method, SipMessage, SipRequest};

use crate::observability::metrics::Face;
use crate::self_gate::{AdmitDecision, BypassKind};
use crate::strategy::RouteParams;
use crate::trace::emit;

use super::super::ProxyCore;
use super::RouteOutcome;

impl ProxyCore {
    /// The To-tag rule: `msg`, received on `face` and routed by `cookie` (if
    /// any), names a dialog no verified cookie backs, so a fresh selection
    /// admits it as a new call. Only an INVITE on the external face.
    pub(super) fn admits_as_new(
        &self,
        msg: &SipMessage,
        face: Face,
        cookie: Option<&RouteParams>,
    ) -> bool {
        let SipMessage::Request(req) = msg else { return false };
        face == Face::External
            && req.method() == Method::Invite
            && req.to().tag().is_some_and(|t| !t.is_empty())
            && cookie.is_none_or(|c| self.strategy.verified_cookie(c, msg).is_none())
    }

    /// Admit `req` as a new call. `Err` holds the outcome of the 503 a refused
    /// one was answered with.
    pub(super) async fn admit_new_call(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        is_emergency: bool,
    ) -> Result<(), RouteOutcome> {
        if is_emergency {
            self.self_gate.note_bypass(BypassKind::Emergency);
            return Ok(());
        }
        let AdmitDecision::Reject { reason, retry_after_sec } = self.self_gate.try_admit_external()
        else {
            return Ok(());
        };
        let reason = reason.label();
        // The shed fact precedes the 503 it explains: the refusal is itself an
        // emission site (the datagram it synthesizes).
        emit::shed(&self.traces, req.call_id().as_str(), self.now_ms() as i64, reason);
        // A closed set: the gate's `ShedReason` labels.
        Err(self.refuse_new(req, src, retry_after_sec, reason).await)
    }
}
