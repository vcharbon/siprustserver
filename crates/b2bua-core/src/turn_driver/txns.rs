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
    /// The transactions of the re-INVITEs among them: INVITEs sent inside a
    /// dialog, under a To-tag.
    reinvites: BTreeSet<Key>,
}

impl ClientTxns {
    /// The transactions of `invite`, sent as the call was dialled.
    pub(super) fn of(invite: &SipRequest) -> Self {
        let mut txns = Self::default();
        txns.open_one(invite);
        txns
    }

    /// The transactions of the requests `result` sends.
    pub(super) fn sent(&mut self, result: &HandlerResult) {
        for req in requests(result) {
            self.open_one(req);
        }
    }

    /// The transaction `req` opens.
    fn open_one(&mut self, req: &SipRequest) {
        let Some(key) = key_of_request(req) else { return };
        if req.method() == Method::Invite && req.to().tag().is_some() {
            self.reinvites.insert(key.clone());
        }
        self.open.insert(key);
    }

    /// The transactions of the requests `result` sent, which a copy whose
    /// write was lost opened: none of them is this node's.
    pub(super) fn lost(&mut self, result: &HandlerResult) {
        for key in requests(result).filter_map(key_of_request) {
            self.open.remove(&key);
        }
    }

    /// Whether `event` is a response a client transaction of this node
    /// matches.
    pub(super) fn matches(&self, event: &CallEvent) -> bool {
        response(event).and_then(key_of_response).is_some_and(|k| self.open.contains(&k))
    }

    /// Whether `event` is a final on a completed client transaction, which
    /// absorbs it: a repeat of its final, or a 2xx to a re-INVITE that took a
    /// non-2xx (RFC 6026 §8.4). A dialling INVITE's hands that 2xx up.
    pub(super) fn absorbs(&self, event: &CallEvent) -> bool {
        let Some(resp) = response(event) else { return false };
        let Some(key) = key_of_response(resp).filter(|k| self.completed.contains(k)) else {
            return false;
        };
        completes(resp) || (resp.status() >= 200 && self.reinvites.contains(&key))
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

/// The requests `result` sends.
fn requests(result: &HandlerResult) -> impl Iterator<Item = &SipRequest> + '_ {
    result.effects.outbound.iter().filter_map(|e| match &e.body {
        OutboundBody::Request(r) => Some(r),
        _ => None,
    })
}

/// The transaction `req` opens, its branch folded: a branch compares ignoring
/// case (RFC 3261 §7.3.1).
fn key_of_request(req: &SipRequest) -> Option<Key> {
    Some((req.top_via().branch()?.to_ascii_lowercase(), req.method().as_str().to_string()))
}

/// The transaction `resp` answers, keyed as [`key_of_request`] keys.
fn key_of_response(resp: &SipResponse) -> Option<Key> {
    Some((resp.top_via().branch()?.to_ascii_lowercase(), resp.cseq().method().as_str().to_string()))
}
