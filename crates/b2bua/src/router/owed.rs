//! What the router owes the sender of a request its call's queue discarded
//! unrun, so no request is left unanswered: the request comes back with its
//! [`Owed`] (the dispatch row's), rendered here from the request itself
//! ([`OwedAnswer`]).

use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::{Method, SipMessage, SipRequest, SipResponse};
use sip_txn::{IdGen, ServerTxnKey, TransactionLayer};

use super::admit;
use super::responses::{build_200, build_481, build_retry_later_500};
use super::RouterCtx;
use crate::admission::{class_of, Refusals, Refused};
use crate::dispatch::{Discard, Owed};
use crate::metrics::B2buaMetrics;
use crate::new_calls::Refusal;
use b2bua_sdk::event::CallEvent;

/// Renders the owed answer of a discarded request through its server
/// transaction, which then absorbs its retransmissions and, for an INVITE,
/// the ACK. A transaction that already holds its final — a CANCEL's 487 got
/// there first — keeps it, and the layer drops this one (RFC 3261 §17.2.1);
/// it is counted all the same.
pub(super) struct OwedAnswer<'a> {
    pub(super) txn: &'a TransactionLayer,
    pub(super) id_gen: &'a Arc<IdGen>,
    pub(super) metrics: &'a B2buaMetrics,
    pub(super) refusals: &'a Refusals,
    pub(super) retry_after_base_sec: u32,
    pub(super) retry_after_jitter_sec: u32,
}

impl<'a> OwedAnswer<'a> {
    pub(super) fn of(ctx: &'a RouterCtx) -> Self {
        Self {
            txn: &ctx.txn,
            id_gen: &ctx.id_gen,
            metrics: &ctx.metrics,
            refusals: &ctx.refusals,
            retry_after_base_sec: ctx.config.retry_after_base_sec,
            retry_after_jitter_sec: ctx.config.retry_after_jitter_sec,
        }
    }

    /// Pay what `event`, discarded for `why`, is owed, and count it. Only a
    /// request is ever owed something. `admitted` when it is a new INVITE the
    /// router's rungs admitted; a new INVITE that is not is a copy of a call
    /// already here, and its refusal is counted as a copy.
    pub(super) async fn render(&self, event: &CallEvent, why: Discard, owed: Owed, admitted: bool) {
        let CallEvent::Sip { message, src, .. } = event else { return };
        let SipMessage::Request(req) = message.as_ref() else { return };
        match owed {
            Owed::Nothing => {}
            Owed::Forget => {
                if let Some(key) = TxnKey::of(req) {
                    key.forget(self.txn);
                }
            }
            Owed::NewCallRefused => {
                // Only the router's run loop opens a queue, with no await
                // between the shed rung and the offer, so an admitted INVITE
                // never meets the global cap here; a copy, offered unjudged,
                // may, and is counted as a copy whatever the discard.
                debug_assert!(
                    !(admitted && why == Discard::AtCap),
                    "an admitted new INVITE passed the shed rung"
                );
                let refused = Refused { reason: Refusal::DispatchDiscard, not_before_sec: 0 };
                admit::answer(self.txn, self.refusals, req, *src, refused).await;
                if admitted {
                    self.metrics.new_calls().reject(refused.reason, class_of(req));
                } else {
                    self.metrics.new_calls().refuse_copy();
                }
            }
            Owed::RetryLater => {
                let resp = build_retry_later_500(req, None, self.jittered_retry_after());
                self.send(resp, *src).await;
                self.metrics.bump_invite_discard_answered(why);
            }
            Owed::DialogGone => {
                self.send(build_481(req, None), *src).await;
                self.metrics.bump_invite_discard_answered(why);
            }
            Owed::CappedRefusal => {
                self.send(self.capped_refusal(req), *src).await;
                self.metrics.bump_capped_request_answered();
            }
        }
    }

    async fn send(&self, resp: SipResponse, dst: SocketAddr) {
        let _ = self.txn.send_response(resp, dst).await;
    }

    /// A Retry-After, in seconds, jittered over the configured base.
    pub(super) fn jittered_retry_after(&self) -> u32 {
        let roll = u64::from(self.id_gen.new_sequence_number());
        load_shed::retry_after::jittered(
            self.retry_after_base_sec,
            self.retry_after_jitter_sec,
            || roll,
        )
    }

    /// A capped call's refusal of a non-INVITE request.
    fn capped_refusal(&self, req: &SipRequest) -> SipResponse {
        match req.method() {
            Method::Bye => build_200(req),
            // The layer binds the tag of the INVITE's final, if any.
            Method::Cancel => build_481(req, None),
            _ => {
                let to_tag = req.to().tag().is_none().then(|| self.id_gen.new_tag());
                build_481(req, to_tag.as_deref())
            }
        }
    }
}

/// A request's server transaction (RFC 3261 §17.2.3: top-Via branch, sent-by
/// and method) and its dialog identity (Call-ID, From-tag).
pub(super) struct TxnKey {
    server: ServerTxnKey,
    call_id: String,
    from_tag: String,
}

impl TxnKey {
    pub(super) fn of(req: &SipRequest) -> Option<Self> {
        ServerTxnKey::of(req).map(|server| TxnKey {
            server,
            call_id: req.call_id().as_str().to_string(),
            from_tag: req.from().tag().unwrap_or_default().to_string(),
        })
    }

    /// Forget the transaction while it has sent nothing
    /// ([`TransactionLayer::forget_unanswered`]); a no-op once it has.
    pub(super) fn forget(&self, txn: &TransactionLayer) {
        txn.forget_unanswered(&self.server, &self.call_id, &self.from_tag);
    }
}

#[cfg(test)]
mod tests;
