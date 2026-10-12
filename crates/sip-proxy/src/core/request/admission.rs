//! The proxy-self gate (ELU/CPS admission) on the request path: a request the
//! ladder takes as a new call meets the gate, and a refused one is answered
//! 503 with Retry-After. An emergency call bypasses the gate.
//!
//! A new call is an initial INVITE (no To-tag), first transmission only, that
//! is not worker-outbound. On the external face an INVITE with a To-tag that
//! no verified cookie backs is a new call too (ADR-0048): it is never refused
//! for its To-tag, only by the gate, as any new call.

use std::net::SocketAddr;

use sip_message::SipRequest;

use crate::self_gate::{AdmitDecision, BypassKind};
use crate::trace::emit;

use super::super::ProxyCore;
use super::reply::{extra_header, proxy_reason, reject_retry_after};
use super::RouteOutcome;

impl ProxyCore {
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
        let extra = [
            extra_header(reject_retry_after(retry_after_sec)),
            extra_header(proxy_reason(503, reason)),
        ];
        // The shed fact precedes the 503 it explains: `reply` is itself an
        // emission site (the datagram it synthesizes).
        emit::shed(&self.traces, req.call_id().as_str(), self.now_ms() as i64, reason);
        // A closed set: the gate's `ShedReason` labels.
        Err(self.reply_reject(req, src, 503, "Service Unavailable", &extra, reason).await)
    }
}
