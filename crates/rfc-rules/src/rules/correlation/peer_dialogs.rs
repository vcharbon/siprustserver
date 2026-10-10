//! The per-endpoint dialog state most of the family reads, and the finding shape
//! every rule of it produces.

use std::collections::HashMap;

use sip_message::sniff;

use crate::verdict::{Decision, Finding, RuleId};
use crate::wire::{Kind, Msg};

/// What ONE endpoint knows about ONE Call-ID from its own stream.
#[derive(Default)]
pub(super) struct PeerDialog {
    /// Tags this endpoint minted: the From-tag of requests it sent, the To-tag
    /// of responses it sent. A received in-dialog message names the dialog with
    /// one of these.
    pub(super) local_tags: Vec<String>,
    /// `(CSeq method, CSeq number)` of every request it sent.
    pub(super) sent_requests: Vec<(String, u32)>,
    /// Whether it has seen the peer's tag: below that, nothing names a dialog.
    pub(super) remote_tagged: bool,
    /// `(top-Via branch, Request-URI)` of every INVITE it took. §9.1 scopes a
    /// CANCEL to ONE INVITE transaction, and a dialog carries several (the
    /// initial INVITE plus re-INVITEs), so the CANCEL rules match by branch
    /// rather than pinning the first INVITE seen.
    pub(super) open_invites: Vec<(Option<String>, Option<String>)>,
    /// The From URI of the first INVITE it took — the URI its peer is bound to.
    pub(super) peer_uri: Option<String>,
    /// It both sent and took an INVITE here: a relay, not a dialog endpoint.
    sent_invite: bool,
    took_invite: bool,
}

impl PeerDialog {
    /// Forwarding both directions of this dialog — see the family doc.
    pub(super) fn relays(&self) -> bool {
        self.sent_invite && self.took_invite
    }

    /// Fold in a message this endpoint SENT.
    fn sent(&mut self, msg: &Msg) {
        match &msg.kind {
            Kind::Request { method } => {
                self.sent_requests.push((msg.cseq_method.to_ascii_uppercase(), msg.cseq));
                if method.eq_ignore_ascii_case("INVITE") {
                    self.sent_invite = true;
                }
                if let Some(t) = &msg.from_tag {
                    self.local_tags.push(t.clone());
                }
            }
            Kind::Response { .. } => {
                if let Some(t) = &msg.to_tag {
                    self.local_tags.push(t.clone());
                }
            }
        }
    }

    /// Fold in a message this endpoint TOOK.
    fn took(&mut self, msg: &Msg) {
        if msg.is_request("INVITE") {
            self.took_invite = true;
            let head = msg.head.as_deref();
            self.open_invites.push((msg.via_branch.clone(), head.and_then(sniff::request_uri)));
            if self.peer_uri.is_none() {
                self.peer_uri = head.and_then(|h| sniff::name_addr_uri(h, "From"));
            }
        }
        let tag = match &msg.kind {
            Kind::Response { .. } => &msg.to_tag,
            Kind::Request { .. } => &msg.from_tag,
        };
        if tag.is_some() {
            self.remote_tagged = true;
        }
    }
}

/// The dialog state EVERY endpoint in a view builds, keyed
/// `(endpoint, Call-ID)`. A rule walks the view and reads the taker's entry as
/// it stood before the message it is judging.
#[derive(Default)]
pub(super) struct PeerDialogs {
    by: HashMap<(String, String), PeerDialog>,
}

impl PeerDialogs {
    /// The taker's state for this message's dialog, as it stands now.
    pub(super) fn taker_of(&self, msg: &Msg) -> Option<&PeerDialog> {
        self.by.get(&(msg.dst.clone(), msg.call_id.clone()))
    }

    /// Fold `msg` in at BOTH ends: its sender minted what it states, its taker
    /// learned what it carried.
    pub(super) fn note(&mut self, msg: &Msg) {
        if msg.call_id.is_empty() {
            return;
        }
        self.by.entry((msg.src.clone(), msg.call_id.clone())).or_default().sent(msg);
        self.by.entry((msg.dst.clone(), msg.call_id.clone())).or_default().took(msg);
    }
}

/// A finding on the message at `mi`, charging the endpoint that emitted it and
/// read at the endpoint that took it.
pub(super) fn charge(rule: RuleId, msg: &Msg, mi: usize, decision: Decision) -> Finding {
    Finding {
        rule,
        emitter: msg.src.clone(),
        taker: msg.dst.clone(),
        cseq: msg.cseq,
        relayed: false,
        anchor: mi,
        decision,
    }
}

/// The `Undecidable` reason a relaying vantage states.
pub(super) const RELAYED: &str = "this vantage relays both directions of the dialog";
