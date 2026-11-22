//! The RFC 3261 §12 dialog table of one UA, fed by every message it sends and
//! receives, and the judgement it passes on each received in-dialog request.
//!
//! A dialog is keyed by its ID as the UA sees it: Call-ID, local tag, remote
//! tag (§12). It is early after a 1xx with a To-tag, confirmed after a 2xx,
//! ended by a non-2xx final to its INVITE (the early dialogs of that INVITE
//! end with it, §12.1.2), or ended by a BYE once the BYE's transaction took
//! its final (§15); a subscription the UA opened on it keeps its NOTIFYs
//! legal past that BYE (RFC 5057 §5.2). A non-2xx final the UA sent makes the
//! peer's requests on the dialog findings only once the peer shows it saw
//! that final; till then the UA still refuses them, and judges them as on the
//! early dialog. It makes a 2xx to a request the UA sent on the dialog a
//! finding only when that request reached the peer behind the final: sent
//! after it, or, on a run that declares a loss model, every copy ahead of it
//! lost ([`AheadOfFinal`]). A request reaching a dialog a BYE or a
//! final ended, whichever side sent it, is a finding unless, on a run that
//! declares a loss model, the wire record shows it a retransmission whose
//! lost first copy left before its sender knew of that end
//! ([`SentBeforeEnd`]); the UA refuses it either way. A CSeq out of the
//! remote sequence that a loss the wire record shows explains, on such a run,
//! is answered as ever and no finding ([`LostOnTheWay`]). It keeps the last
//! remote CSeq: as UAS it starts at the
//! initial INVITE's CSeq, as UAC it is empty until the peer's first request
//! (§12.1.1, §12.1.2).

use std::collections::{BTreeSet, HashMap, HashSet};

use rfc_rules::rules::correlation::EARLY_DIALOG_WINDOW_US;
use sip_message::header;
use sip_message::{SipRequest, SipResponse};

use super::verdict::{DialogVerdict, VerdictKind};
use super::wire_ahead::AheadOfFinal;
use super::wire_crossing::{DialogEnd, Ending, SentBeforeEnd};
use super::wire_loss::LostOnTheWay;
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

impl std::fmt::Display for DialogId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Call-ID {} local {} remote {}", self.call_id, self.local, self.remote)
    }
}

impl std::fmt::Display for DialogState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialogState::Early => f.write_str("early"),
            DialogState::Confirmed => f.write_str("confirmed"),
            DialogState::EndedByFinal(status) => write!(f, "ended by a {status} final"),
            DialogState::EndedByBye => f.write_str("ended by a BYE"),
        }
    }
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
    /// How many Via entries it carried as the UA received it; `None` for one
    /// the UA sent. The caller's ACK of a non-2xx final arrives with as many
    /// (RFC 3261 §17.1.1.3, §16.6 step 8), a hop's own ACK of it with one
    /// fewer at least.
    received_vias: Option<usize>,
}

#[derive(Debug, Clone)]
struct Dialog {
    state: DialogState,
    /// The initial INVITE that created it.
    invite: InviteKey,
    remote_cseq: Option<u32>,
    /// The CSeq of the last BYE the UA sent on it.
    pending_bye: Option<u32>,
    /// The final or the BYE that ended it, the first of them; `None` while it
    /// is early or confirmed.
    end: Option<DialogEnd>,
    /// Subscriptions the UA opened on it (a REFER's implicit one, RFC 3515
    /// §2.4.4, or a SUBSCRIBE) not yet terminated: a BYE ends the invite
    /// usage only, and their NOTIFYs stay legal (RFC 5057 §5.2).
    subscriptions: u32,
    /// The `(CSeq, method, top-Via branch)` of every request received on it:
    /// a repeat of one is a retransmission of its transaction, judged once.
    received: BTreeSet<(u32, String, String)>,
    /// The `(CSeq, method)` of every 2xx the UA received on it to a request it
    /// sent: a repeat of one is its server transaction's replay (RFC 3261
    /// §17.2.2), judged once.
    answered: BTreeSet<(u32, String)>,
    /// The `(CSeq, method)` of every request the UA first sent on it while it
    /// was early or confirmed. The UA's messages are taken to reach the peer
    /// in the order it sent them, so each reached the peer ahead of a final
    /// the UA sent after it.
    sent_live: BTreeSet<(u32, String)>,
    /// Whether the UA, as UAS, ended it early with a non-2xx final the peer
    /// has not provably seen (one the final opens is seen with its tag):
    /// neither the caller's ACK of that final (RFC 3261
    /// §17.2.1) nor a new INVITE of the caller (§8.1.3.5) reached the UA yet,
    /// so a request the peer sent on it may cross the final. The peer's
    /// messages are taken to reach the UA in the order it sent them: one sent
    /// before that ACK and delivered after it is judged as sent after.
    final_unseen: bool,
}

impl Dialog {
    /// The transaction of the BYE numbered `cseq`, which the peer sent when
    /// `by_peer`, took its final: it ends the dialog unless a BYE ended it
    /// before.
    fn end_by_bye(&mut self, cseq: u32, by_peer: bool) {
        if self.state != DialogState::EndedByBye {
            self.end = Some(DialogEnd { ending: Ending::Bye { cseq }, by_peer });
        }
        self.state = DialogState::EndedByBye;
    }
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
    /// What the wire record shows of a request reaching a dialog a final or
    /// a BYE ended, on a run that declares a loss model; with none, every
    /// such request is a finding (the UA's own final unseen by the peer
    /// excepted).
    sent_before_end: Option<SentBeforeEnd>,
    /// What the wire record shows of the requests lost on their way to the
    /// UA, on a run that declares a loss model; with none, no loss explains
    /// a CSeq out of the remote sequence.
    lost_on_the_way: Option<LostOnTheWay>,
    /// What the wire record shows of a request the UA sent reaching the peer
    /// ahead of the UA's own final, on a run that declares a loss model; with
    /// none, the UA's send order (`Dialog::sent_live`) tells.
    ahead_of_final: Option<AheadOfFinal>,
}

/// Whether a REFER or SUBSCRIBE opens a subscription: a REFER stating
/// `Refer-Sub: false` asks for none (RFC 4488 §4).
fn opens_subscription(request: &SipRequest) -> bool {
    request.method().as_str() != "REFER"
        || !request
            .header::<header::ReferSub>()
            .and_then(Result::ok)
            .is_some_and(|refer_sub| refer_sub.is("false"))
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

    /// Read from `wire` whether a request reaching a dialog a final or a BYE
    /// ended crossed that end.
    pub fn set_sent_before_end(&mut self, wire: SentBeforeEnd) {
        self.sent_before_end = Some(wire);
    }

    /// Read from `wire` which requests the network lost on their way to the
    /// UA: a CSeq out of the remote sequence they explain is a
    /// [`VerdictKind::LossExplained`].
    pub fn set_lost_on_the_way(&mut self, wire: LostOnTheWay) {
        self.lost_on_the_way = Some(wire);
    }

    /// Read from `wire` whether a request the UA sent reached the peer ahead
    /// of the UA's own final, in place of the UA's send order.
    pub fn set_ahead_of_final(&mut self, wire: AheadOfFinal) {
        self.ahead_of_final = Some(wire);
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

    /// Every dialog the UA holds, one line each, ordered by dialog ID:
    /// `Call-ID c local x remote y: confirmed, last remote CSeq 2`.
    pub fn rows(&self) -> Vec<String> {
        let mut ids: Vec<&DialogId> = self.dialogs.keys().collect();
        ids.sort_by(|a, b| {
            (&a.call_id, &a.local, &a.remote).cmp(&(&b.call_id, &b.local, &b.remote))
        });
        ids.into_iter()
            .map(|id| {
                let d = &self.dialogs[id];
                let cseq = match d.remote_cseq {
                    Some(c) => format!("last remote CSeq {c}"),
                    None => "no remote CSeq yet".to_string(),
                };
                format!("{}: {}, {cseq}", id, d.state)
            })
            .collect()
    }

    /// The dialogs the UA holds early or confirmed, one ID each, ordered.
    pub fn live(&self) -> Vec<String> {
        let mut live: Vec<String> = self
            .dialogs
            .iter()
            .filter(|(_, d)| matches!(d.state, DialogState::Early | DialogState::Confirmed))
            .map(|(id, d)| format!("{id} ({})", d.state))
            .collect();
        live.sort();
        live
    }

    /// Feed a request the UA sent.
    pub fn note_sent_request(&mut self, request: &SipRequest) {
        let call_id = request.call_id().to_string();
        let Some(local) = tag_of(request.from().tag()) else { return };
        let method = request.method().as_str();
        let cseq = request.cseq().seq();
        let Some(remote) = tag_of(request.to().tag()) else {
            if method == "INVITE" {
                let key = InviteKey { call_id, caller_tag: local, role: Role::Uac };
                self.invites.entry(key).and_modify(|i| i.cseq = cseq).or_insert(Invite {
                    cseq,
                    answered_at_us: None,
                    received_vias: None,
                });
            }
            return;
        };
        let Some(d) = self.dialogs.get_mut(&DialogId { call_id, local, remote }) else { return };
        if matches!(d.state, DialogState::Early | DialogState::Confirmed) {
            d.sent_live.insert((cseq, method.to_string()));
        }
        match method {
            "BYE" => d.pending_bye = Some(cseq),
            "REFER" | "SUBSCRIBE" if opens_subscription(request) => d.subscriptions += 1,
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
                    d.end_by_bye(response.cseq().seq(), true);
                }
            }
            _ if status >= 300 && !self.dialogs.contains_key(&id) => {
                self.refused.insert(id);
            }
            _ => {}
        }
    }

    /// Feed a response the UA received (it is the UAC of the request) and
    /// judge it: `Some` when it is a 2xx to an in-dialog request the UA sent
    /// on a dialog a non-2xx final ended by the time it arrives (RFC 3261
    /// §12.1.2: that dialog is gone, so nothing on it is answered 2xx). A
    /// PRACK is excepted (RFC 3262 §3), and a CANCEL is hop-by-hop. A final
    /// the UA sent ends the dialog for this judgement only for a request that
    /// reached the peer behind it ([`Self::reached_ahead`]), and a repeat of
    /// a 2xx already taken on the dialog is not judged again.
    pub fn note_received_response(
        &mut self,
        response: &SipResponse,
        now_us: u64,
    ) -> Option<DialogVerdict> {
        let (Some(local), Some(remote)) =
            (tag_of(response.from().tag()), tag_of(response.to().tag()))
        else {
            return None;
        };
        let id = DialogId { call_id: response.call_id().to_string(), local, remote };
        let status = response.status();
        let method = response.cseq().method().as_str();
        let initial = method == "INVITE" && {
            let key = InviteKey {
                call_id: id.call_id.clone(),
                caller_tag: id.local.clone(),
                role: Role::Uac,
            };
            self.invites.get(&key).is_some_and(|i| i.cseq == response.cseq().seq())
        };
        let judged =
            (200..300).contains(&status) && !initial && !matches!(method, "PRACK" | "CANCEL");
        let ahead = self.dialogs.get(&id).is_some_and(|d| self.reached_ahead(d, response));
        if let Some(d) = self.dialogs.get_mut(&id).filter(|_| judged) {
            let first = d.answered.insert((response.cseq().seq(), method.to_string()));
            if let (true, false, DialogState::EndedByFinal(by)) = (first, ahead, d.state) {
                return Some(DialogVerdict {
                    method: method.to_string(),
                    call_id: id.call_id.clone(),
                    from_tag: id.local.clone(),
                    to_tag: id.remote.clone(),
                    cseq: response.cseq().seq(),
                    terminating: false,
                    kind: VerdictKind::EndedAnswered { status, by },
                });
            }
        }
        match method {
            "INVITE" if initial => {
                let key = InviteKey {
                    call_id: id.call_id.clone(),
                    caller_tag: id.local.clone(),
                    role: Role::Uac,
                };
                self.invite_response(key, id, status, now_us);
            }
            "BYE" if status >= 200 => {
                if let Some(d) = self.dialogs.get_mut(&id) {
                    if d.pending_bye == Some(response.cseq().seq()) {
                        d.end_by_bye(response.cseq().seq(), false);
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
        None
    }

    /// Whether the request `answer` answers on `dialog` reached the peer
    /// ahead of the UA's own non-2xx final that ended the dialog: on a run
    /// that declares a loss model as the wire record shows it
    /// ([`AheadOfFinal`]), else by the UA's send order (`Dialog::sent_live`).
    /// `false` on a dialog no final of the UA's ended.
    fn reached_ahead(&self, dialog: &Dialog, answer: &SipResponse) -> bool {
        let Some(DialogEnd { ending: Ending::Final { invite_cseq, status }, by_peer: false }) =
            dialog.end
        else {
            return false;
        };
        match &self.ahead_of_final {
            Some(wire) => wire.holds(answer, invite_cseq, status),
            None => {
                let cseq = answer.cseq();
                dialog.sent_live.contains(&(cseq.seq(), cseq.method().as_str().to_string()))
            }
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
        let final_end = DialogEnd {
            ending: Ending::Final { invite_cseq: invite.cseq, status },
            by_peer: key.role == Role::Uac,
        };
        let fresh = |state| Dialog {
            state,
            invite: key.clone(),
            remote_cseq,
            pending_bye: None,
            end: matches!(state, DialogState::EndedByFinal(_)).then_some(final_end),
            subscriptions: 0,
            received: BTreeSet::new(),
            answered: BTreeSet::new(),
            sent_live: BTreeSet::new(),
            final_unseen: false,
        };
        match status {
            101..=199 => {
                self.dialogs.entry(id).or_insert_with(|| fresh(DialogState::Early));
            }
            200..=299 => {
                invite.answered_at_us.get_or_insert(now_us);
                let d = self.dialogs.entry(id).or_insert_with(|| fresh(DialogState::Confirmed));
                // A 2xx confirms the dialog its To-tag names. One a non-2xx
                // final seemed to end was answered on another transaction of
                // the same INVITE identity (a merged copy, RFC 3261 §8.2.2.2,
                // drew that final); only a BYE's end is final.
                if matches!(d.state, DialogState::Early | DialogState::EndedByFinal(_)) {
                    d.state = DialogState::Confirmed;
                    d.end = None;
                }
            }
            300.. => {
                for d in self.dialogs.values_mut() {
                    if d.invite == key && d.state == DialogState::Early {
                        d.state = DialogState::EndedByFinal(status);
                        d.end = Some(final_end);
                        d.final_unseen = key.role == Role::Uas;
                    }
                }
                self.dialogs.entry(id).or_insert_with(|| fresh(DialogState::EndedByFinal(status)));
            }
            _ => {}
        }
    }

    /// Whether `ack` on `dialog` is the caller's own ACK of the non-2xx final
    /// the UA, as UAS, ended it with (RFC 3261 §17.2.1): its INVITE's number
    /// and as many Via entries as that INVITE carried when the UA received
    /// it. A transaction-stateful hop's own ACK of the final (§17.1.1.3)
    /// carries one Via fewer at least, and shows nothing the caller saw.
    fn acks_own_final(&self, dialog: &Dialog, ack: &SipRequest) -> bool {
        dialog.invite.role == Role::Uas
            && matches!(dialog.state, DialogState::EndedByFinal(_))
            && self.invites.get(&dialog.invite).is_some_and(|i| {
                i.cseq == ack.cseq().seq() && i.received_vias == Some(ack.via().len())
            })
    }

    /// The peer showed it saw the final the UA sent to the INVITE `key`: the
    /// early dialogs that final ended are ended for it too.
    fn final_seen(&mut self, key: &InviteKey) {
        for d in self.dialogs.values_mut().filter(|d| d.invite == *key) {
            d.final_unseen = false;
        }
    }

    /// Feed a request the UA received and judge it: `Some` when it names no
    /// dialog the UA holds, a dialog that ended (a PRACK excepted on a dialog
    /// a non-2xx final ended), or a CSeq out of the remote sequence (RFC 3261
    /// §12.2.2). One on a dialog the UA's own final ended before the peer
    /// provably saw it is a [`VerdictKind::Crossed`], refused, judged as on
    /// the still early dialog and a finding only of a rule it broke there
    /// (`Dialog::final_unseen`); so is one on a dialog a final or a BYE ended,
    /// whichever side sent it, that the wire record shows a retransmission
    /// whose lost first copy left before the peer knew of that end
    /// ([`SentBeforeEnd`]), judged as on the dialog before it. A CSeq out
    /// of sequence that a loss on the way to the UA explains is a
    /// [`VerdictKind::LossExplained`] ([`LostOnTheWay`]). A request
    /// with no To-tag is out of
    /// dialog and only teaches the table an initial INVITE. An ACK or a
    /// CANCEL carries its INVITE's number, so only its dialog is judged, and
    /// a retransmission of a request already taken is not judged again. The
    /// ACK of a non-2xx final the UA sent, or a new INVITE of the same caller
    /// at a higher CSeq, shows the peer saw that final.
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
                let vias = Some(request.via().len());
                let retry = self.invites.get(&key).is_some_and(|i| cseq > i.cseq);
                self.invites
                    .entry(key.clone())
                    .and_modify(|i| {
                        i.cseq = cseq;
                        i.received_vias = vias;
                    })
                    .or_insert(Invite { cseq, answered_at_us: None, received_vias: vias });
                if retry {
                    self.final_seen(&key);
                }
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
            terminating: method == "NOTIFY" && notify_terminates(request),
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
        if method == "ACK" && self.acks_own_final(dialog, request) {
            let key = dialog.invite.clone();
            self.final_seen(&key);
            return None;
        }
        let txn = (cseq, method.clone(), top_via_branch(request).unwrap_or_default());
        if !sequenced || dialog.received.contains(&txn) {
            return None;
        }
        let state = dialog.state;
        let final_unseen = dialog.final_unseen;
        let subscribed = method == "NOTIFY" && dialog.subscriptions > 0;
        let end = dialog.end;
        let dialog = self.dialogs.get_mut(&id).expect("looked up above");
        dialog.received.insert(txn);
        if subscribed && notify_terminates(request) {
            dialog.subscriptions -= 1;
        }
        let crossed_end =
            || end.zip(self.sent_before_end.as_ref()).is_some_and(|(end, w)| w.holds(request, end));
        let crossed_by = match state {
            // A NOTIFY of a subscription the UA opened outlives the BYE that
            // ended the invite usage (RFC 5057 §5.2).
            DialogState::EndedByBye if subscribed => None,
            // A PRACK names a reliable provisional, and its transaction does
            // not end with the INVITE's (RFC 3262 §3, §4): one owed before the
            // final may follow it.
            DialogState::EndedByFinal(_) if method == "PRACK" => None,
            DialogState::EndedByFinal(by) if final_unseen || crossed_end() => Some(Some(by)),
            DialogState::EndedByFinal(status) => {
                return Some(verdict(VerdictKind::Ended { by: Some(status) }))
            }
            DialogState::EndedByBye if crossed_end() => Some(None),
            DialogState::EndedByBye => return Some(verdict(VerdictKind::Ended { by: None })),
            DialogState::Early | DialogState::Confirmed => None,
        };
        let early = state == DialogState::Early || matches!(crossed_by, Some(Some(_)));
        let branch = top_via_branch(request);
        let broke = self
            .judge_in_sequence(&id, &method, cseq, early, now_us)
            .map(|kind| self.explained_by_loss(&id, &method, branch.as_deref(), kind));
        match crossed_by {
            Some(by) => Some(verdict(VerdictKind::Crossed { by, broke: broke.map(Box::new) })),
            None => broke.map(verdict),
        }
    }

    /// The rule a request numbered `cseq` on the dialog `id` breaks, judged
    /// as on an early dialog when `early`: an early dialog another branch's
    /// 2xx abandoned (§13.2.2.4), a re-INVITE on an early dialog (§14.1: its
    /// INVITE is in progress), a callee's BYE on one (§15), a CSeq out of the
    /// remote sequence (§12.2.2). A CSeq not going backwards becomes the last
    /// remote CSeq.
    fn judge_in_sequence(
        &mut self,
        id: &DialogId,
        method: &str,
        cseq: u32,
        early: bool,
        now_us: u64,
    ) -> Option<VerdictKind> {
        let dialog = self.dialogs.get(id)?;
        let abandoned = early
            && self.invites.get(&dialog.invite).and_then(|i| i.answered_at_us).is_some_and(|at| {
                now_us > at.saturating_add(EARLY_DIALOG_WINDOW_US)
                    || matches!(method, "BYE" | "INVITE")
            });
        if abandoned {
            return Some(self.unknown(id));
        }
        let role = dialog.invite.role;
        if early && (method == "INVITE" || (method == "BYE" && role == Role::Uac)) {
            return Some(VerdictKind::OnEarlyDialog { reinvite: method == "INVITE" });
        }
        let backwards = self.backwards;
        let dialog = self.dialogs.get_mut(id)?;
        match dialog.remote_cseq {
            Some(last) if cseq < last => match backwards {
                BackwardsCseq::Verdict => Some(VerdictKind::CseqBackwards { cseq, last }),
                BackwardsCseq::Accept => None,
            },
            Some(last) if cseq > last.saturating_add(1) => {
                dialog.remote_cseq = Some(cseq);
                Some(VerdictKind::CseqSkip { cseq, last })
            }
            _ => {
                dialog.remote_cseq = Some(cseq);
                None
            }
        }
    }

    /// `kind`, the rule a `method` request of top-Via `branch` on the dialog
    /// `id` broke, as a [`VerdictKind::LossExplained`] when the wire record
    /// shows the network lost on its way to the UA a copy of the request
    /// itself, its CSeq, method and branch, for a CSeq gone backwards (this
    /// is a retransmission of it, overtaken); or, for a CSeq skipping, a
    /// request numbered each number of the gap (RFC 3261 §12.2.2,
    /// §17.1.2.2). Any other kind, or one no loss explains, stays.
    fn explained_by_loss(
        &self,
        id: &DialogId,
        method: &str,
        branch: Option<&str>,
        kind: VerdictKind,
    ) -> VerdictKind {
        let Some(wire) = &self.lost_on_the_way else { return kind };
        let lost = || wire.lost(&id.call_id, &id.remote, &id.local);
        let explained = match kind {
            VerdictKind::CseqBackwards { cseq, .. } => {
                lost().iter().any(|l| l.is_copy_of(cseq, method, branch))
            }
            VerdictKind::CseqSkip { cseq, last } => {
                let lost = lost();
                (last + 1..cseq).all(|n| {
                    lost.iter()
                        .any(|l| l.cseq == n && !matches!(l.method.as_str(), "ACK" | "CANCEL"))
                })
            }
            _ => false,
        };
        match explained {
            true => VerdictKind::LossExplained { broke: Box::new(kind) },
            false => kind,
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
