//! The RFC 3261 §12 dialog table of one UA, fed by every message it sends and
//! receives, and the judgement it passes on each received in-dialog request.
//!
//! A dialog is keyed by its ID as the UA sees it: Call-ID, local tag, remote
//! tag (§12). It is early after a 1xx with a To-tag, confirmed after a 2xx,
//! ended by a non-2xx final to its INVITE (the early dialogs of that INVITE
//! end with it, §12.1.2), or ended by a BYE once the BYE's transaction took
//! its final (§15); a subscription the UA opened on it keeps its NOTIFYs
//! legal past that BYE (RFC 5057 §5.2). It keeps the last remote CSeq: as UAS
//! it starts at the initial INVITE's CSeq, as UAC it is empty until the peer's
//! first request (§12.1.1, §12.1.2).

use std::collections::{BTreeSet, HashMap, HashSet};

use rfc_rules::rules::correlation::EARLY_DIALOG_WINDOW_US;
use sip_message::header;
use sip_message::{SipRequest, SipResponse};

use super::verdict::{DialogVerdict, VerdictKind};
use crate::agent::top_via_branch;

/// What a CSeq below the last remote CSeq draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackwardsCseq {
    /// A [`VerdictKind::CseqBackwards`] (RFC 3261 §12.2.2).
    #[default]
    Verdict,
    /// Nothing: the request is taken as it comes.
    Accept,
}

/// The state of one dialog (RFC 3261 §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogState {
    Early,
    Confirmed,
    /// A non-2xx final to its INVITE ended it, with this status.
    EndedByFinal(u16),
    /// A BYE's transaction ended it.
    EndedByBye,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DialogId {
    call_id: String,
    local: String,
    remote: String,
}

/// Which side of an initial INVITE the UA is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Role {
    Uac,
    Uas,
}

/// An initial INVITE the UA sent or received: the Call-ID and the tag every
/// dialog it creates shares (the UAC's From-tag), keyed with the UA's role.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InviteKey {
    call_id: String,
    caller_tag: String,
    role: Role,
}

#[derive(Debug, Clone)]
struct Invite {
    cseq: u32,
    /// When the first 2xx to it was sent or received, on the table's clock.
    answered_at_us: Option<u64>,
}

#[derive(Debug, Clone)]
struct Dialog {
    state: DialogState,
    /// The initial INVITE that created it.
    invite: InviteKey,
    remote_cseq: Option<u32>,
    /// The CSeq of a BYE the UA sent on it whose final has not arrived.
    pending_bye: Option<u32>,
    /// Subscriptions the UA opened on it (a REFER's implicit one, RFC 3515
    /// §2.4.4, or a SUBSCRIBE) not yet terminated: a BYE ends the invite
    /// usage only, and their NOTIFYs stay legal (RFC 5057 §5.2).
    subscriptions: u32,
    /// The `(CSeq, method, top-Via branch)` of every request received on it:
    /// a repeat of one is a retransmission of its transaction, judged once.
    received: BTreeSet<(u32, String, String)>,
}

/// The dialog table of one UA.
#[derive(Debug, Default)]
pub struct DialogTable {
    backwards: BackwardsCseq,
    invites: HashMap<InviteKey, Invite>,
    dialogs: HashMap<DialogId, Dialog>,
    /// Dialog IDs the UA answered a request on without holding them (its
    /// `481`): the hop ACK of such a refused INVITE names them legitimately.
    refused: HashSet<DialogId>,
}

/// Whether a NOTIFY ends its subscription (`Subscription-State: terminated`,
/// RFC 6665 §4.1.3).
fn notify_terminates(request: &SipRequest) -> bool {
    request
        .header::<header::SubscriptionState>()
        .and_then(Result::ok)
        .is_some_and(|state| state.is("terminated"))
}

fn tag_of(tag: Option<&str>) -> Option<String> {
    tag.filter(|t| !t.is_empty()).map(str::to_string)
}

impl DialogTable {
    pub fn new(backwards: BackwardsCseq) -> Self {
        DialogTable { backwards, ..DialogTable::default() }
    }

    /// What a CSeq below the last remote CSeq draws from now on.
    pub fn set_backwards(&mut self, backwards: BackwardsCseq) {
        self.backwards = backwards;
    }

    /// The state of the dialog `(call_id, local, remote)`, if the UA holds it.
    pub fn state(&self, call_id: &str, local: &str, remote: &str) -> Option<DialogState> {
        let id = DialogId {
            call_id: call_id.to_string(),
            local: local.to_string(),
            remote: remote.to_string(),
        };
        self.dialogs.get(&id).map(|d| d.state)
    }

    /// Feed a request the UA sent.
    pub fn note_sent_request(&mut self, request: &SipRequest) {
        let call_id = request.call_id().to_string();
        let Some(local) = tag_of(request.from().tag()) else { return };
        let method = request.method().as_str();
        match tag_of(request.to().tag()) {
            None if method == "INVITE" => {
                let key = InviteKey { call_id, caller_tag: local, role: Role::Uac };
                let cseq = request.cseq().seq();
                self.invites
                    .entry(key)
                    .and_modify(|i| i.cseq = cseq)
                    .or_insert(Invite { cseq, answered_at_us: None });
            }
            Some(remote) if method == "BYE" => {
                if let Some(d) = self.dialogs.get_mut(&DialogId { call_id, local, remote }) {
                    d.pending_bye = Some(request.cseq().seq());
                }
            }
            Some(remote) if matches!(method, "REFER" | "SUBSCRIBE") => {
                if let Some(d) = self.dialogs.get_mut(&DialogId { call_id, local, remote }) {
                    d.subscriptions += 1;
                }
            }
            _ => {}
        }
    }

    /// Feed a response the UA sent (it is the UAS of the request).
    pub fn note_sent_response(&mut self, response: &SipResponse, now_us: u64) {
        let (Some(local), Some(remote)) =
            (tag_of(response.to().tag()), tag_of(response.from().tag()))
        else {
            return;
        };
        let id = DialogId { call_id: response.call_id().to_string(), local, remote };
        let status = response.status();
        match response.cseq().method().as_str() {
            "INVITE" => {
                let key = InviteKey {
                    call_id: id.call_id.clone(),
                    caller_tag: id.remote.clone(),
                    role: Role::Uas,
                };
                let initial =
                    self.invites.get(&key).is_some_and(|i| i.cseq == response.cseq().seq());
                if initial {
                    self.invite_response(key, id, status, now_us);
                } else if status >= 300 && !self.dialogs.contains_key(&id) {
                    self.refused.insert(id);
                }
            }
            "BYE" if (200..300).contains(&status) => {
                if let Some(d) = self.dialogs.get_mut(&id) {
                    d.state = DialogState::EndedByBye;
                }
            }
            _ if status >= 300 && !self.dialogs.contains_key(&id) => {
                self.refused.insert(id);
            }
            _ => {}
        }
    }

    /// Feed a response the UA received (it is the UAC of the request).
    pub fn note_received_response(&mut self, response: &SipResponse, now_us: u64) {
        let (Some(local), Some(remote)) =
            (tag_of(response.from().tag()), tag_of(response.to().tag()))
        else {
            return;
        };
        let id = DialogId { call_id: response.call_id().to_string(), local, remote };
        let status = response.status();
        match response.cseq().method().as_str() {
            "INVITE" => {
                let key = InviteKey {
                    call_id: id.call_id.clone(),
                    caller_tag: id.local.clone(),
                    role: Role::Uac,
                };
                if self.invites.get(&key).is_some_and(|i| i.cseq == response.cseq().seq()) {
                    self.invite_response(key, id, status, now_us);
                }
            }
            "BYE" if status >= 200 => {
                if let Some(d) = self.dialogs.get_mut(&id) {
                    if d.pending_bye == Some(response.cseq().seq()) {
                        d.state = DialogState::EndedByBye;
                    }
                }
            }
            "REFER" | "SUBSCRIBE" if status >= 300 => {
                if let Some(d) = self.dialogs.get_mut(&id) {
                    d.subscriptions = d.subscriptions.saturating_sub(1);
                }
            }
            _ => {}
        }
    }

    /// A response to an initial INVITE, sent or received under the dialog ID
    /// `id`: a 1xx opens an early dialog, a 2xx confirms one, a non-2xx final
    /// ends the INVITE's early dialogs (RFC 3261 §12.1, §13.2.2.4).
    fn invite_response(&mut self, key: InviteKey, id: DialogId, status: u16, now_us: u64) {
        let Some(invite) = self.invites.get_mut(&key) else { return };
        // As UAS the remote sequence starts at the INVITE's CSeq; as UAC it
        // starts empty (§12.1.1, §12.1.2).
        let remote_cseq = (key.role == Role::Uas).then_some(invite.cseq);
        let fresh = |state| Dialog {
            state,
            invite: key.clone(),
            remote_cseq,
            pending_bye: None,
            subscriptions: 0,
            received: BTreeSet::new(),
        };
        match status {
            101..=199 => {
                self.dialogs.entry(id).or_insert_with(|| fresh(DialogState::Early));
            }
            200..=299 => {
                invite.answered_at_us.get_or_insert(now_us);
                let d = self.dialogs.entry(id).or_insert_with(|| fresh(DialogState::Confirmed));
                if d.state == DialogState::Early {
                    d.state = DialogState::Confirmed;
                }
            }
            300.. => {
                for d in self.dialogs.values_mut() {
                    if d.invite == key && d.state == DialogState::Early {
                        d.state = DialogState::EndedByFinal(status);
                    }
                }
                self.dialogs.entry(id).or_insert_with(|| fresh(DialogState::EndedByFinal(status)));
            }
            _ => {}
        }
    }

    /// Feed a request the UA received and judge it: `Some` when it names no
    /// dialog the UA holds, a dialog that ended (a PRACK excepted on a dialog
    /// a non-2xx final ended), or a CSeq out of the remote sequence (RFC 3261
    /// §12.2.2). A request with no To-tag is out of
    /// dialog and only teaches the table an initial INVITE. An ACK or a
    /// CANCEL carries its INVITE's number, so only its dialog is judged, and
    /// a retransmission of a request already taken is not judged again.
    pub fn judge_received(&mut self, request: &SipRequest, now_us: u64) -> Option<DialogVerdict> {
        let call_id = request.call_id().to_string();
        let method = request.method().as_str().to_string();
        let remote = tag_of(request.from().tag()).unwrap_or_default();
        let Some(local) = tag_of(request.to().tag()) else {
            if method == "INVITE" {
                // A new INVITE of the same caller (an authentication retry)
                // is a new transaction: its responses are the initial ones.
                let key = InviteKey { call_id, caller_tag: remote, role: Role::Uas };
                let cseq = request.cseq().seq();
                self.invites
                    .entry(key)
                    .and_modify(|i| i.cseq = cseq)
                    .or_insert(Invite { cseq, answered_at_us: None });
            }
            return None;
        };
        let cseq = request.cseq().seq();
        let verdict = |kind| DialogVerdict {
            method: method.clone(),
            call_id: call_id.clone(),
            from_tag: remote.clone(),
            to_tag: local.clone(),
            cseq,
            kind,
        };
        let id =
            DialogId { call_id: call_id.clone(), local: local.clone(), remote: remote.clone() };
        let sequenced = !matches!(method.as_str(), "ACK" | "CANCEL");
        let Some(dialog) = self.dialogs.get(&id) else {
            if !sequenced && self.refused.contains(&id) {
                return None;
            }
            return Some(verdict(self.unknown(&id)));
        };
        let txn = (cseq, method.clone(), top_via_branch(request).unwrap_or_default());
        if !sequenced || dialog.received.contains(&txn) {
            return None;
        }
        let abandoned = dialog.state == DialogState::Early
            && self.invites.get(&dialog.invite).and_then(|i| i.answered_at_us).is_some_and(|at| {
                now_us > at.saturating_add(EARLY_DIALOG_WINDOW_US)
                    || matches!(method.as_str(), "BYE" | "INVITE")
            });
        let state = dialog.state;
        let last = dialog.remote_cseq;
        let subscribed = method == "NOTIFY" && dialog.subscriptions > 0;
        let dialog = self.dialogs.get_mut(&id).expect("looked up above");
        dialog.received.insert(txn);
        if subscribed && notify_terminates(request) {
            dialog.subscriptions -= 1;
        }
        match state {
            // A NOTIFY of a subscription the UA opened outlives the BYE that
            // ended the invite usage (RFC 5057 §5.2).
            DialogState::EndedByBye if subscribed => {}
            // A PRACK names a reliable provisional, and its transaction does
            // not end with the INVITE's (RFC 3262 §3, §4): one owed before the
            // final may follow it.
            DialogState::EndedByFinal(_) if method == "PRACK" => {}
            DialogState::EndedByFinal(status) => {
                return Some(verdict(VerdictKind::Ended { by: Some(status) }))
            }
            DialogState::EndedByBye => return Some(verdict(VerdictKind::Ended { by: None })),
            DialogState::Early | DialogState::Confirmed => {}
        }
        if abandoned {
            return Some(verdict(self.unknown(&id)));
        }
        let dialog = self.dialogs.get_mut(&id).expect("looked up above");
        match last {
            Some(last) if cseq < last => match self.backwards {
                BackwardsCseq::Verdict => Some(verdict(VerdictKind::CseqBackwards { cseq, last })),
                BackwardsCseq::Accept => None,
            },
            Some(last) if cseq > last.saturating_add(1) => {
                dialog.remote_cseq = Some(cseq);
                Some(verdict(VerdictKind::CseqSkip { cseq, last }))
            }
            _ => {
                dialog.remote_cseq = Some(cseq);
                None
            }
        }
    }

    /// The tag of `id` the UA does not hold, and the ones it holds in its
    /// place: the remote tag when the local tag names a dialog of this
    /// Call-ID, else the local tag.
    fn unknown(&self, id: &DialogId) -> VerdictKind {
        let live = |d: &&DialogId| {
            d.call_id == id.call_id
                && self
                    .dialogs
                    .get(*d)
                    .is_some_and(|x| matches!(x.state, DialogState::Early | DialogState::Confirmed))
        };
        let mut remotes: Vec<String> = self
            .dialogs
            .keys()
            .filter(live)
            .filter(|d| d.local == id.local && d.remote != id.remote)
            .map(|d| d.remote.clone())
            .collect();
        if !remotes.is_empty() || self.dialogs.keys().any(|d| d.local == id.local) {
            remotes.sort();
            return VerdictKind::UnknownDialog {
                header: "From",
                unknown: id.remote.clone(),
                held: remotes,
            };
        }
        let mut locals: Vec<String> =
            self.dialogs.keys().filter(live).map(|d| d.local.clone()).collect();
        locals.sort();
        locals.dedup();
        VerdictKind::UnknownDialog { header: "To", unknown: id.local.clone(), held: locals }
    }
}
