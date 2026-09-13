//! The To-tag every outbound response is held to (RFC 3261 §8.2.6.2, §9.2,
//! §12.1.1): once a final has bound this node's tag to an INVITE server
//! transaction — the dialog's local tag — nothing the TU hands over for that
//! transaction, or for its CANCEL, leaves under another one. Enforced below
//! the rules and the router on purpose: a defect there is corrected on the
//! wire, counted, and loud in a debug build.

use std::sync::atomic::Ordering::Relaxed;

use sip_message::generators::response::{is_fallback_to_tag, retag_response};
use sip_message::{Method, SipResponse};

use crate::event::TxnKind;
use crate::timers::{ms, TIMER_L};

use super::owner::Owner;
use super::txn::TxnRole;

/// What the layer holds one response to.
enum Bound {
    /// The tag it carries, whatever the TU chose.
    Exact(String),
    /// The tag it takes where the TU chose none.
    Fill(String),
    /// Nothing bound yet: the TU's tag stands and may become the bound one.
    Free,
}

impl Owner {
    /// `response` under the tag this layer binds it to. A response to a
    /// CANCEL, or on a transaction whose request's To carried a tag, is
    /// re-rendered when its tag is not the bound one; a response carrying no
    /// usable tag takes the bound one; a response nothing binds leaves as it
    /// came. `status <= 100` carries no tag and is never touched.
    pub(super) fn bind_to_tag(&mut self, response: SipResponse) -> SipResponse {
        if response.status() <= 100 {
            return response;
        }
        let carried = response.to().tag().map(str::to_string);
        let fallback = carried.as_deref().is_some_and(is_fallback_to_tag);
        let chosen = carried.as_deref().filter(|_| !fallback);
        let bound = match (self.bound_for(&response), chosen) {
            (Bound::Free, _) | (Bound::Fill(_), Some(_)) => {
                if fallback {
                    self.metrics.fallback_to_tag_used.fetch_add(1, Relaxed);
                }
                return response;
            }
            (Bound::Exact(bound), Some(tag)) if tag == bound => return response,
            (Bound::Exact(bound), Some(tag)) => {
                self.metrics.to_tag_coerced.fetch_add(1, Relaxed);
                tracing::warn!(
                    status = response.status(),
                    cseq = %response.cseq().method(),
                    carried = tag,
                    bound,
                    "response handed over under a To-tag other than the bound one; re-rendered"
                );
                debug_assert!(
                    !self.strict_to_tag,
                    "{} to {} handed over under To-tag {tag}, bound {bound}",
                    response.status(),
                    response.cseq().method()
                );
                bound
            }
            (Bound::Exact(bound) | Bound::Fill(bound), None) => {
                self.metrics.to_tag_filled.fetch_add(1, Relaxed);
                bound
            }
        };
        retag_response(&response, &bound).unwrap_or(response)
    }

    fn bound_for(&self, response: &SipResponse) -> Bound {
        let branch = response.top_via().branch().unwrap_or_default();
        let txn = self.txns.get(branch).filter(|t| t.role == TxnRole::Server);
        // A CANCEL shares its INVITE's branch (§9.1) and its answer that
        // INVITE's tag (§9.2): the one the INVITE's own To named (§8.2.6.2),
        // else the one the transaction bound, else the one remembered past it.
        if *response.cseq().method() == Method::Cancel {
            return txn
                .and_then(|t| t.original_request.as_ref())
                .and_then(|r| r.to().tag().map(str::to_string))
                .or_else(|| txn.and_then(|t| t.bound_to_tag().map(str::to_string)))
                .or_else(|| self.recall_uas_tag(response))
                .map_or(Bound::Free, Bound::Exact);
        }
        let Some(txn) = txn else {
            return self.recall_uas_tag(response).map_or(Bound::Free, Bound::Fill);
        };
        // A request that named the dialog is answered under that tag (§8.2.6.2).
        if let Some(tag) = txn.original_request.as_ref().and_then(|r| r.to().tag()) {
            return Bound::Exact(tag.to_string());
        }
        // An INVITE still open: each provisional may open an early dialog and
        // the final binds the tag (§12.1.1) — the TU chooses. A second final
        // never reaches here (a transaction that sent its final drops it,
        // through Completed and Confirmed).
        if txn.kind == TxnKind::Invite {
            return Bound::Free;
        }
        txn.bound_to_tag()
            .map(str::to_string)
            .or_else(|| self.recall_uas_tag(response))
            .map_or(Bound::Free, Bound::Fill)
    }

    /// Record what a final this layer just sent on a server INVITE
    /// transaction bound: the dialog's tag, remembered past the transaction.
    /// A provisional binds nothing here — the first one's tag is pinned as
    /// `uas_to_tag` by the sender.
    pub(super) fn record_uas_tag(&mut self, branch: &str, response: &SipResponse) {
        let Some(tag) = response.to().tag().map(str::to_string) else { return };
        let (call_id, from_tag) = {
            let Some(txn) = self.txns.get_mut(branch) else { return };
            if txn.role != TxnRole::Server || txn.kind != TxnKind::Invite || response.status() < 200
            {
                return;
            }
            txn.final_to_tag = Some(tag.clone());
            (txn.call_id.clone(), txn.from_tag.clone())
        };
        self.remember_uas_tag(&call_id, &from_tag, &tag);
    }

    /// Keep the tag this node bound to the dialog `(call_id, from_tag)` for
    /// Timer L, so an answer composed after its transaction is gone still
    /// carries it.
    pub(super) fn remember_uas_tag(&mut self, call_id: &str, from_tag: &str, tag: &str) {
        self.recent_uas_tags.insert(
            (call_id.to_string(), from_tag.to_string()),
            (tag.to_string(), tokio::time::Instant::now()),
        );
    }

    /// The tag remembered for the dialog `response` answers on, while it is
    /// still within Timer L.
    fn recall_uas_tag(&self, response: &SipResponse) -> Option<String> {
        let from_tag = response.from().tag()?;
        let key = (response.call_id().as_str().to_string(), from_tag.to_string());
        let (tag, since) = self.recent_uas_tags.get(&key)?;
        (since.elapsed() < ms(TIMER_L)).then(|| tag.clone())
    }
}
