//! The PRACKs a released call still answers (RFC 3262 §3): a PRACK's server
//! transaction is not the INVITE's and does not end with it, so a PRACK naming
//! a reliable provisional this stack showed, reaching it after the call was
//! released, draws 200 rather than the 481 of a request naming nothing.
//!
//! A call released with shown provisionals its party never PRACKed leaves
//! their `RAck` tokens here, keyed by the shown dialog's `Call-ID` and this
//! stack's tag in it, for 64·T1 — the §3 bound past which no UAS waits for a
//! PRACK. The book is node-local: a late PRACK landing on another node after a
//! failover draws that node's 481.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use call::helpers::UnackedShown;
use call::Call;
use sip_message::header::RAck;
use sip_message::{Method, SipRequest, SipResponse};

use super::responses::build_200;
use super::RouterCtx;

/// How long a released call's shown provisionals stay answerable: RFC 3262
/// §3's 64·T1 give-up bound.
const WINDOW_MS: i64 = 64 * sip_retransmit::timers::T1 as i64;

type DialogKey = (String, String);

/// Each dialog's `RAck` tokens under the instant they stop being answerable,
/// and the expiries in the order they fall; a dialog kept again holds its
/// latest expiry, and an older queue entry for it removes nothing.
#[derive(Default)]
struct Inner {
    racks: HashMap<DialogKey, (i64, Vec<(i64, i64)>)>,
    expiry: VecDeque<(i64, DialogKey)>,
}

impl Inner {
    fn prune(&mut self, now_ms: i64) {
        while self.expiry.front().is_some_and(|(at, _)| *at <= now_ms) {
            let Some((at, key)) = self.expiry.pop_front() else { break };
            if self.racks.get(&key).is_some_and(|(until, _)| *until == at) {
                self.racks.remove(&key);
            }
        }
    }
}

/// The released calls' unacknowledged shown provisionals, answerable for
/// [`WINDOW_MS`] after the release.
#[derive(Default)]
pub struct LatePrackBook {
    inner: Mutex<Inner>,
}

impl LatePrackBook {
    /// Keep what `call`, released at `now_ms`, showed and was never PRACKed.
    pub fn remember(&self, call: &Call, now_ms: i64) {
        self.keep(call::helpers::unacknowledged_shown(call), now_ms);
    }

    /// Whether `req` is a PRACK naming, on every §7.2 token, a provisional a
    /// released call showed in the dialog the PRACK's `To` tag names.
    pub fn answers(&self, req: &SipRequest, now_ms: i64) -> bool {
        if req.method() != Method::Prack {
            return false;
        }
        let Some(tag) = req.to().tag() else { return false };
        let Some(rack) = req.header::<RAck>().and_then(Result::ok) else { return false };
        *rack.method() == Method::Invite
            && self.names(
                req.call_id().as_str(),
                tag,
                (i64::from(rack.rseq()), i64::from(rack.seq())),
                now_ms,
            )
    }

    fn keep(&self, shown: Vec<UnackedShown>, now_ms: i64) {
        if shown.is_empty() {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else { return };
        inner.prune(now_ms);
        let until = now_ms + WINDOW_MS;
        for s in shown {
            let key = (s.call_id, s.tag);
            inner.racks.insert(key.clone(), (until, s.racks));
            inner.expiry.push_back((until, key));
        }
    }

    fn names(&self, call_id: &str, tag: &str, rack: (i64, i64), now_ms: i64) -> bool {
        let Ok(mut inner) = self.inner.lock() else { return false };
        inner.prune(now_ms);
        let key = (call_id.to_string(), tag.to_string());
        inner.racks.get(&key).is_some_and(|(_, racks)| racks.contains(&rack))
    }
}

/// The 200 `req` draws when it is a PRACK the book answers, counted as such
/// (`b2bua_late_prack_answered_total`); `None` for any other request, which
/// takes the answer of a request naming no call.
pub(super) fn answer(ctx: &RouterCtx, req: &SipRequest) -> Option<SipResponse> {
    if !ctx.late_pracks.answers(req, ctx.clock.now_ms()) {
        return None;
    }
    ctx.metrics.bump_late_prack_answered();
    Some(build_200(req))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown() -> Vec<UnackedShown> {
        vec![UnackedShown { call_id: "a-call".into(), tag: "shown".into(), racks: vec![(7, 3)] }]
    }

    #[test]
    fn a_shown_provisional_is_answerable_inside_the_window_only() {
        let book = LatePrackBook::default();
        book.keep(shown(), 1_000);
        assert!(book.names("a-call", "shown", (7, 3), 1_000 + WINDOW_MS - 1));
        assert!(!book.names("a-call", "shown", (7, 3), 1_000 + WINDOW_MS));
    }

    #[test]
    fn a_dialog_kept_again_keeps_its_later_window() {
        let book = LatePrackBook::default();
        book.keep(shown(), 0);
        book.keep(shown(), 1_000);
        assert!(book.names("a-call", "shown", (7, 3), WINDOW_MS + 1), "the later window holds");
        assert!(!book.names("a-call", "shown", (7, 3), 1_000 + WINDOW_MS));
    }

    #[test]
    fn a_prack_naming_anything_else_is_not_answerable() {
        let book = LatePrackBook::default();
        book.keep(shown(), 0);
        assert!(!book.names("a-call", "shown", (8, 3), 1), "another RSeq");
        assert!(!book.names("a-call", "shown", (7, 2), 1), "another INVITE");
        assert!(!book.names("a-call", "other", (7, 3), 1), "another dialog");
        assert!(!book.names("b-call", "shown", (7, 3), 1), "another call");
    }
}
