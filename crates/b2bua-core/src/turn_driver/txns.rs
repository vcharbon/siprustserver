//! What this node's transaction layer holds of the requests a driven call
//! sent: the client transactions a response matches, by its Via branch and
//! its CSeq method (a CANCEL shares its INVITE's branch, RFC 3261 §9.1), and
//! the completed ones that absorb a repeat of their final (§17.1).

use std::collections::BTreeSet;

use b2bua_sdk::event::CallEvent;
use sip_message::{Method, SipMessage, SipRequest, SipResponse};

use crate::effects::{HandlerResult, OutboundBody};

/// A client transaction: its branch and its method.
type Key = (String, String);

/// The client transactions of the requests this node sent.
#[derive(Clone, Debug, Default)]
pub(super) struct ClientTxns {
    /// The transactions of the requests this node sent.
    open: BTreeSet<Key>,
    /// The transactions a final completed: a non-INVITE's, or an INVITE's
    /// non-2xx (§17.1.1.2, §17.1.2.2).
    completed: BTreeSet<Key>,
}

impl ClientTxns {
    /// The transactions of `invite`, sent as the call was dialled.
    pub(super) fn of(invite: &SipRequest) -> Self {
        let mut txns = Self::default();
        txns.open.extend(key_of_request(invite));
        txns
    }

    /// The transactions of the requests `result` sends.
    pub(super) fn sent(&mut self, result: &HandlerResult) {
        self.open.extend(keys(result));
    }

    /// The transactions of the requests `result` sent, which a copy whose
    /// write was lost opened: none of them is this node's.
    pub(super) fn lost(&mut self, result: &HandlerResult) {
        for key in keys(result) {
            self.open.remove(&key);
        }
    }

    /// Whether `event` is a response a client transaction of this node
    /// matches.
    pub(super) fn matches(&self, event: &CallEvent) -> bool {
        response(event).and_then(key_of_response).is_some_and(|k| self.open.contains(&k))
    }

    /// Whether `event` repeats the final of a completed client transaction,
    /// which absorbs it.
    pub(super) fn absorbs(&self, event: &CallEvent) -> bool {
        let completed =
            |r: &SipResponse| key_of_response(r).is_some_and(|k| self.completed.contains(&k));
        response(event).is_some_and(|r| completes(r) && completed(r))
    }

    /// The transaction `event` answers, completed by its final.
    pub(super) fn received(&mut self, event: &CallEvent) {
        if !self.matches(event) {
            return;
        }
        let Some(resp) = response(event).filter(|r| completes(r)) else { return };
        self.completed.extend(key_of_response(resp));
    }
}

/// Whether `resp` completes its client transaction: any final to a
/// non-INVITE, a non-2xx final to an INVITE (a 2xx's repeats reach the
/// transaction user, RFC 6026 §7.2).
fn completes(resp: &SipResponse) -> bool {
    let status = resp.status();
    status >= 200 && (resp.cseq().method() != Method::Invite || status >= 300)
}

/// The response `event` carries, if any.
fn response(event: &CallEvent) -> Option<&SipResponse> {
    let CallEvent::Sip { message, .. } = event else { return None };
    match message.as_ref() {
        SipMessage::Response(resp) => Some(resp),
        SipMessage::Request(_) => None,
    }
}

/// The transactions of the requests `result` sends.
fn keys(result: &HandlerResult) -> impl Iterator<Item = Key> + '_ {
    result.effects.outbound.iter().filter_map(|e| match &e.body {
        OutboundBody::Request(r) => key_of_request(r),
        _ => None,
    })
}

/// The transaction `req` opens.
fn key_of_request(req: &SipRequest) -> Option<Key> {
    Some((req.top_via().branch()?.to_string(), req.method().as_str().to_string()))
}

/// The transaction `resp` answers.
fn key_of_response(resp: &SipResponse) -> Option<Key> {
    Some((resp.top_via().branch()?.to_string(), resp.cseq().method().as_str().to_string()))
}
