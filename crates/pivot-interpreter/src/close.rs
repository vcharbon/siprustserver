//! The **generic close** (`PCAP2TEST_PIVOT_V3.md` §11.2): what a scripted
//! endpoint owes when its SCRIPT ends before its CALL does.
//!
//! A run that cannot go on ends its script, not its call — whatever the
//! document's polarity. The flow stops where it stopped — the failure stands,
//! the recording stays verbatim — and every scripted leg then ends what it holds
//! by the RFC's own rules: it answers a request it took and never answered,
//! acknowledges a final
//! it took (RFC 3261 §13.2.2.4, §17.1.1.3), closes the dialog it OPENED with a
//! BYE (§15), and cancels an INVITE it sent that has a provisional behind it and
//! no final (§9.1).
//!
//! **What the platform owes, the platform sends.** A leg that ANSWERED an INVITE
//! never starts the teardown: the far side closes such a dialog and this end
//! answers what arrives. A platform that then tears nothing down leaves the call
//! up, and the settle contract (§10) says so — which is a finding about the
//! system, not something a scripted peer papers over.
//!
//! **Read off the RECORDING, never off the document.** An obligation is derived
//! from the datagrams that crossed the leg, so a shape this program has never
//! seen closes the same way and no document scripts its own close. The emission
//! itself is the leg's own [`LegStack`](crate::stack::LegStack), so what the
//! close puts on the wire is ordinary compliant SIP.

use std::collections::{BTreeMap, BTreeSet};

use pivot_schema::bundle::{CloseOwed, Dir, RecordedMessage};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser, SipRequest, SipResponse};

use crate::recording::Recording;
use crate::stack;

/// What one scripted leg still owes once its script has ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owed {
    /// Nothing this leg can do: it holds nothing open, or its dialog is over.
    Nothing,
    /// The INVITE this leg sent has drawn no final yet — nothing at all, so
    /// RFC 3261 §9.1 cancels nothing, or a CANCEL is already out and the 487 it
    /// draws is still coming. Either way that final is still owed an ACK, so the
    /// leg is not done.
    AwaitFinal,
    /// This leg ANSWERED the dialog, so the far side closes it (RFC 3261 §15)
    /// and this end answers whatever teardown arrives.
    AwaitTeardown,
    /// A request this leg took and never answered with a final: the
    /// transaction it names (CSeq and the dialog's To-tag), the status, and
    /// — for a request that named no dialog — the tag of the early dialog the
    /// answer rides (RFC 3261 §8.2.6.2), where one is ending it.
    Answer {
        cseq_method: String,
        cseq: u32,
        to_tag: Option<String>,
        status: u16,
        early_tag: Option<String>,
    },
    /// A final this leg took and never acknowledged (RFC 3261 §13.2.2.4 for a
    /// 2xx, §17.1.1.3 for anything else).
    Ack(Box<SipResponse>),
    /// The dialog this leg opened, established and acknowledged.
    Bye,
    /// The INVITE this leg sent, with a provisional behind it and no final.
    Cancel,
}

impl Owed {
    /// The act this obligation puts on the wire, where it puts one. `None` is a
    /// leg with nothing to do NOW — either nothing at all, or something only the
    /// far side can move.
    pub fn act(&self) -> Option<CloseOwed> {
        match self {
            Owed::Nothing | Owed::AwaitFinal | Owed::AwaitTeardown => None,
            Owed::Answer { .. } => Some(CloseOwed::Answer),
            Owed::Ack(_) => Some(CloseOwed::Ack),
            Owed::Bye => Some(CloseOwed::Bye),
            Owed::Cancel => Some(CloseOwed::Cancel),
        }
    }

    /// Whether this leg is still part of the close — it owes an act, or it is
    /// waiting for one the far side must send first.
    pub fn open(&self) -> bool {
        *self != Owed::Nothing
    }
}

impl std::fmt::Display for Owed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Owed::Nothing => f.write_str("holds nothing open"),
            Owed::AwaitFinal => f.write_str("sent an INVITE no final has answered yet"),
            Owed::AwaitTeardown => f.write_str(
                "answered the dialog, and the teardown its far side owes has not \
                             arrived",
            ),
            Owed::Answer { cseq_method, status, .. } => {
                write!(f, "owes a {status} to the {cseq_method} it took")
            }
            Owed::Ack(response) => {
                write!(f, "owes an ACK to the {} it took", response.status())
            }
            Owed::Bye => f.write_str("owes the BYE that ends the dialog it opened"),
            Owed::Cancel => f.write_str("owes a CANCEL for the INVITE it sent"),
        }
    }
}

/// What every leg the run recorded still owes.
pub fn obligations(recording: &Recording) -> BTreeMap<String, Owed> {
    recording.legs().iter().map(|(leg, messages)| (leg.clone(), owed(messages))).collect()
}

/// One leg's dialog and transaction state, as its own ladder states it.
#[derive(Default)]
struct LegView {
    /// The CSeq of the INVITE this leg SENT, where it sent one.
    sent_invite: Option<u32>,
    /// Whether anything answered that INVITE, provisionally or finally.
    heard_response: bool,
    /// The final that answered it, where one has.
    took_final: Option<SipResponse>,
    /// The CSeq numbers this leg has ACKed.
    acked: BTreeSet<u32>,
    /// Whether this leg has already sent a CANCEL for its INVITE.
    sent_cancel: bool,
    /// Whether a BYE crossed this leg in either direction: the dialog is over,
    /// or is being ended by the side that sent it.
    bye_seen: bool,
    /// The tag this leg holds its ESTABLISHED dialog under — the To-tag the
    /// peer's in-dialog requests carry (RFC 3261 §12.2.2) — while the dialog
    /// is up: the tag of the 2xx this leg sent to the INVITE it took, or the
    /// From-tag of the INVITE it sent once a 2xx answered it. The last
    /// confirmed dialog, one per leg, as the stack holds it. Cleared by the
    /// 2xx this leg sends to a taken BYE; a BYE this leg SENDS ends the dialog
    /// only with its final (§15.1.1), so it clears nothing here.
    held_dialog: Option<String>,
    /// The tags of the dialogs this leg ENDED by answering a BYE 2xx: a request
    /// still pending under one of them is owed its 487 (§15.1.2).
    ended: BTreeSet<Option<String>>,
    /// The From-tag of the INVITE this leg sent, before anything answers it.
    own_tag: Option<String>,
    /// The early dialogs this leg is ringing (§12.1.1): the tag of each
    /// provisional it SENT, to the CSeq of the INVITE it answers, until the
    /// 2xx under it, a non-2xx to that INVITE (§12.3), or the 200 this leg
    /// sends to a BYE on it.
    rang: BTreeMap<String, u32>,
    /// The early dialogs a BYE this leg answered 200 ended, by tag, to the
    /// CSeq of the INVITE each still rides: that INVITE is owed its 487
    /// (§15.1.2, last paragraph), under the tag the dialog rang with.
    ended_early: BTreeMap<String, u32>,
    /// The early dialogs the peer is ringing for the INVITE this leg sent
    /// (§12.1.2): the tag of each tagged provisional it took, until the 2xx
    /// under it, any non-2xx (§12.3), or the 200 this leg sends to a BYE on it.
    peer_rang: BTreeSet<String>,
    /// Whether this leg TOOK an INVITE, and the final it answered it with.
    took_invite: bool,
    answered_invite: Option<u16>,
    /// Whether a CANCEL arrived for the INVITE this leg took.
    took_cancel: bool,
    /// Requests taken and not answered with a final, oldest first.
    unanswered: Vec<Open>,
    /// The reliable provisionals this leg SENT and the ones a PRACK it answered
    /// 2xx has acknowledged (RFC 3262 §3).
    sent_reliable: BTreeSet<Reliable>,
    acknowledged: BTreeSet<Reliable>,
    /// What the RAck of every PRACK this leg took names, by transaction.
    took_prack: BTreeMap<Transaction, Option<Reliable>>,
}

/// A request this leg took and has not answered with a final.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Open {
    method: Method,
    transaction: Transaction,
}

/// A server transaction as the dialog it rides names it: the request's CSeq
/// number and its To-tag (RFC 3261 §17.2.3 matches the transaction, §12 the
/// dialog — two forks number their first in-dialog request alike).
type Transaction = (u32, Option<String>);

/// A reliable provisional as its PRACK names it (RFC 3262 §3, §7.2): the
/// early dialog's To-tag, its RSeq and the INVITE CSeq it answers.
type Reliable = (Option<String>, u32, u32);

/// What one leg owes, read off its recorded ladder.
fn owed(messages: &[RecordedMessage]) -> Owed {
    let view = view(messages);

    // A request the peer is still waiting on comes first: an unanswered server
    // transaction retransmits, and every other obligation is behind it.
    if let Some(answer) = view.to_answer() {
        return answer;
    }
    // The dialog this leg OPENED.
    if view.sent_invite.is_some() {
        if let Some(final_response) = &view.took_final {
            if let Some(unacked) = view.to_ack() {
                return Owed::Ack(Box::new(unacked.clone()));
            }
            let established = (200..300).contains(&final_response.status());
            return if established && !view.bye_seen { Owed::Bye } else { Owed::Nothing };
        }
        // No final yet: cancel what has already been answered provisionally, and
        // otherwise wait — for the response §9.1 requires before a CANCEL may be
        // sent at all, or for the final a CANCEL already out will draw. The
        // waiting leg is not DONE: that final still owes an ACK (§17.1.1.3).
        return match (view.sent_cancel, view.heard_response) {
            (false, true) => Owed::Cancel,
            (true, _) | (false, false) => Owed::AwaitFinal,
        };
    }
    // The dialog this leg ANSWERED: the far side closes it.
    if view.took_invite {
        let established = view.answered_invite.is_some_and(|status| (200..300).contains(&status));
        if established && !view.bye_seen {
            return Owed::AwaitTeardown;
        }
    }
    Owed::Nothing
}

impl Open {
    fn answer(&self, status: u16) -> Owed {
        self.answer_under(status, None)
    }

    /// [`answer`](Self::answer) riding the early dialog `early_tag` names.
    fn answer_under(&self, status: u16, early_tag: Option<String>) -> Owed {
        Owed::Answer {
            cseq_method: self.method.as_str().to_string(),
            cseq: self.transaction.0,
            to_tag: self.transaction.1.clone(),
            status,
            early_tag,
        }
    }
}

impl LegView {
    /// The request this leg answers first, and the answer it is owed.
    ///
    /// A CANCEL jumps the queue (RFC 3261 §9.2), then a PRACK (its client
    /// transaction runs a timer, §17.1.2, and a non-2xx INVITE final first
    /// would end the early dialog it rides, §12.3), then the oldest — the one
    /// retransmitting longest. An INVITE on a dialog an answered BYE ended is
    /// a cancelled one (§15.1.2, last paragraph), see [`Self::bye_ended`].
    fn to_answer(&self) -> Option<Owed> {
        let first_of = |method: Method| self.unanswered.iter().find(|open| open.method == method);
        let first = first_of(Method::Cancel).or_else(|| first_of(Method::Prack)).or_else(|| {
            self.unanswered.iter().find(|open| answer_status(&open.method, false).is_some())
        })?;
        let ended = self.bye_ended(first);
        let status = match first.method {
            Method::Prack => self.prack_status(&first.transaction),
            _ => answer_status(&first.method, self.took_cancel || ended.is_some())?,
        };
        Some(first.answer_under(status, ended.flatten()))
    }

    /// Whether the BYE named by `to_tag` is on a dialog this leg holds: the
    /// established one, or an early one — rung by this leg under that tag, or
    /// by the peer under `from_tag` for the INVITE this leg sent.
    fn holds(&self, to_tag: &Option<String>, from_tag: Option<&str>) -> bool {
        let established = self.held_dialog.is_some() && &self.held_dialog == to_tag;
        let rang = to_tag.as_ref().is_some_and(|tag| self.rang.contains_key(tag));
        let peer_rang = self.own_tag.is_some()
            && &self.own_tag == to_tag
            && from_tag.is_some_and(|tag| self.peer_rang.contains(tag));
        established || rang || peer_rang
    }

    /// Where a BYE this leg answered 200 ended the dialog the INVITE `open`
    /// is pending on (§15.1.2, last paragraph): the tag its answer rides where
    /// it named no dialog — the early dialog that rang for it (§8.2.6.2) —
    /// and `None` inside for one that named its dialog itself.
    fn bye_ended(&self, open: &Open) -> Option<Option<String>> {
        if open.method != Method::Invite {
            return None;
        }
        match &open.transaction.1 {
            Some(_) => self.ended.contains(&open.transaction.1).then_some(None),
            None => self
                .ended_early
                .iter()
                .find(|(_, cseq)| **cseq == open.transaction.0)
                .map(|(tag, _)| Some(tag.clone())),
        }
    }

    /// The INVITE still pending on the dialog `to_tag` names once a BYE this
    /// leg answered 200 ended it, owed its 487 (§15.1.2, last paragraph).
    /// Nothing on a dialog no answered BYE ended, and nothing for another
    /// dialog's INVITE.
    fn pending_on_ended(&self, to_tag: &Option<String>) -> Option<Owed> {
        self.unanswered.iter().find_map(|open| {
            let early = self.bye_ended(open)?;
            let dialog = early.as_ref().or(open.transaction.1.as_ref());
            (dialog == to_tag.as_ref()).then(|| open.answer_under(487, early.clone()))
        })
    }

    /// The final a PRACK this leg took is answered with (RFC 3262 §3): 2xx when
    /// its RAck names a reliable provisional this leg sent in the same early
    /// dialog and no PRACK has acknowledged yet, 481 otherwise — a PRACK with
    /// no well-formed RAck names nothing and draws the 481 too.
    fn prack_status(&self, transaction: &Transaction) -> u16 {
        match self.took_prack.get(transaction).cloned().flatten() {
            Some(named)
                if self.sent_reliable.contains(&named) && !self.acknowledged.contains(&named) =>
            {
                200
            }
            _ => 481,
        }
    }

    /// The final this leg took for the INVITE it sent and has not acknowledged
    /// (RFC 3261 §13.2.2.4 for a 2xx, §17.1.1.3 for anything else).
    fn to_ack(&self) -> Option<&SipResponse> {
        let final_response = self.took_final.as_ref()?;
        (!self.acked.contains(&final_response.cseq().seq())).then_some(final_response)
    }
}

/// What one leg owes for a datagram the flow scripts NO step for, whether the
/// flow is still running or has completed.
///
/// The refusal stands: this names only the transaction-layer acts the RFCs make
/// the endpoint's own whatever a document scripts — the `200` a CANCEL is
/// answered with and the `487` the INVITE it names ends with (RFC 3261 §9.2),
/// the ACK a non-2xx INVITE final is owed on its own branch (§17.1.1.3), the
/// final a PRACK draws (RFC 3262 §3), and the final a BYE draws — `200` on
/// a dialog this leg holds, established or early (§15), `481` on none
/// (§15.1.2), then the `487` a request still pending on the dialog that 200
/// ended is owed (§15.1.2, last paragraph). Anything else owes nothing:
/// answering it would put a message on the wire nothing asked for.
///
/// One act per call, off the same ladder the generic close reads — so the
/// CANCEL pair and the BYE pair are two calls each, the second made once the
/// first is on the recording.
pub fn unscripted(messages: &[RecordedMessage], trigger: &SipMessage) -> Option<Owed> {
    let view = view(messages);
    match trigger {
        // RFC 3261 §15.1.2: a BYE on a dialog this leg holds — established,
        // or early (§15 lets a BYE end one) — is answered 200; one matching
        // no dialog of this leg's — another tag, no tag, a dialog a final or
        // an answered BYE already ended — the 481 the same section states,
        // composed from the request (§8.2.6). Once the 200 is out, the dialog
        // is over and a request still pending on it draws 487.
        SipMessage::Request(request) if *request.method() == Method::Bye => {
            let transaction = transaction_of(request);
            let Some(open) = view.unanswered.iter().find(|open| open.transaction == transaction)
            else {
                return view.pending_on_ended(&transaction.1);
            };
            let status = if view.holds(&transaction.1, request.from().tag()) { 200 } else { 481 };
            Some(open.answer(status))
        }
        SipMessage::Request(request) if *request.method() == Method::Cancel => {
            // Only the pair §9.2 makes this CANCEL's own: the CANCEL itself, and
            // then the INVITE it names — whatever else the leg holds open.
            let open = |method| view.unanswered.iter().find(|open| open.method == method);
            match open(Method::Cancel) {
                Some(cancel) => Some(cancel.answer(200)),
                None => open(Method::Invite).map(|invite| invite.answer(487)),
            }
        }
        // RFC 3262 §3: a UAS answers every PRACK — 2xx for the unacknowledged
        // reliable provisional its RAck names, 481 for anything else — so the
        // relay it rode gets its own final and the caller's PRACK is not left
        // to time out on this leg's silence.
        SipMessage::Request(request) if *request.method() == Method::Prack => {
            let transaction = transaction_of(request);
            let open = view.unanswered.iter().find(|open| open.transaction == transaction)?;
            Some(open.answer(view.prack_status(&transaction)))
        }
        SipMessage::Response(response)
            if *response.cseq().method() == Method::Invite && response.status() >= 300 =>
        {
            let unacked = view.to_ack()?;
            (unacked.cseq().seq() == response.cseq().seq())
                .then(|| Owed::Ack(Box::new(unacked.clone())))
        }
        _ => None,
    }
}

/// The final status a request the close answers is answered with, where the
/// method alone decides it. `None` where the RFC states no termination answer
/// for the method: an unanswered INFO or OPTIONS holds no call up, and inventing
/// a response for it would put a message on the wire nothing asked for.
fn answer_status(method: &Method, cancelled: bool) -> Option<u16> {
    match method {
        Method::Bye | Method::Cancel => Some(200),
        // RFC 3261 §9.2: a CANCELled INVITE ends 487. A UAS whose script simply
        // stopped is not going to answer at all, and says so (§21.4.18).
        Method::Invite => Some(if cancelled { 487 } else { 480 }),
        // A PRACK is answered by what its RAck names (`LegView::prack_status`).
        _ => None,
    }
}

/// One leg's state, folded over its ladder in wire order.
fn view(messages: &[RecordedMessage]) -> LegView {
    let mut view = LegView::default();
    for recorded in messages {
        // A byte-identical repeat is the same transaction retransmitting, not a
        // second one: it re-opens nothing a final already ended (RFC 3261
        // §17.2.2, §17.2.1 answer a repeat with the last response or silence).
        if recorded.repeat_of.is_some() {
            continue;
        }
        let Some(message) = parse(&recorded.raw) else { continue };
        match (recorded.dir, message) {
            (Dir::Out, SipMessage::Request(request)) => {
                let cseq = request.cseq().seq();
                match request.method() {
                    Method::Invite => {
                        view.sent_invite = Some(cseq);
                        view.own_tag = request.from().tag().map(str::to_string);
                    }
                    Method::Ack => {
                        view.acked.insert(cseq);
                    }
                    Method::Cancel => view.sent_cancel = true,
                    Method::Bye => view.bye_seen = true,
                    _ => {}
                }
            }
            (Dir::In, SipMessage::Request(request)) => {
                let transaction = transaction_of(&request);
                match request.method() {
                    Method::Invite => view.took_invite = true,
                    Method::Cancel => view.took_cancel = true,
                    Method::Bye => view.bye_seen = true,
                    Method::Prack => {
                        view.took_prack.insert(transaction.clone(), rack_of(&request));
                    }
                    _ => {}
                }
                // An ACK answers no transaction of its own, so it is never owed
                // a response; everything else the leg took may be.
                if request.method() != Method::Ack {
                    view.unanswered.push(Open { method: request.method().clone(), transaction });
                }
            }
            (Dir::Out, SipMessage::Response(response)) => {
                let cseq = response.cseq();
                let to_tag = response.to().tag().map(str::to_string);
                if response.status() < 200 {
                    if cseq.method() == Method::Invite {
                        if let Some(tag) = &to_tag {
                            view.rang.insert(tag.clone(), cseq.seq());
                        }
                        if stack::reliably(&response) {
                            if let Some(rseq) = stack::rseq_of(&response) {
                                view.sent_reliable.insert((to_tag, rseq, cseq.seq()));
                            }
                        }
                    }
                    continue;
                }
                // The transaction this final ends. A request that carried no
                // To-tag (an INVITE, the CANCEL it draws) is ended by its CSeq
                // alone; one that did is answered under that tag (§8.2.6.2).
                let ended = |open: &Open| {
                    &open.method == cseq.method()
                        && open.transaction.0 == cseq.seq()
                        && open
                            .transaction
                            .1
                            .as_ref()
                            .is_none_or(|tag| Some(tag) == to_tag.as_ref())
                };
                if cseq.method() == Method::Prack && (200..300).contains(&response.status()) {
                    let transaction = (cseq.seq(), to_tag.clone());
                    if let Some(named) = view.took_prack.get(&transaction).cloned().flatten() {
                        view.acknowledged.insert(named);
                    }
                }
                view.unanswered.retain(|open| !ended(open));
                match cseq.method() {
                    // A 2xx confirms the fork it rode; a non-2xx ends the
                    // INVITE transaction and every early dialog it opened
                    // (§12.3).
                    Method::Invite => {
                        view.answered_invite = Some(response.status());
                        if (200..300).contains(&response.status()) {
                            if let Some(tag) = &to_tag {
                                view.rang.remove(tag);
                            }
                            view.held_dialog = to_tag;
                        } else {
                            view.rang.retain(|_, invite| *invite != cseq.seq());
                        }
                    }
                    // The 2xx to a taken BYE ends the dialog it names on this
                    // side too — established, or early on either side: a
                    // later BYE names a dialog this leg no longer holds, and
                    // a request still pending under it is owed its 487.
                    Method::Bye if (200..300).contains(&response.status()) => {
                        if view.held_dialog == to_tag {
                            view.held_dialog = None;
                        }
                        if let Some((tag, invite)) =
                            to_tag.as_ref().and_then(|tag| view.rang.remove_entry(tag))
                        {
                            view.ended_early.insert(tag, invite);
                        }
                        if let Some(peer) = response.from().tag() {
                            view.peer_rang.remove(peer);
                        }
                        view.ended.insert(to_tag);
                    }
                    // A BYE answered 481 named a dialog already gone: nothing
                    // pending under that tag is this leg's to end.
                    Method::Bye => {
                        view.ended.remove(&to_tag);
                    }
                    _ => {}
                }
            }
            (Dir::In, SipMessage::Response(response)) => {
                let cseq = response.cseq();
                if cseq.method() != Method::Invite || Some(cseq.seq()) != view.sent_invite {
                    continue;
                }
                view.heard_response = true;
                let peer_tag = response.to().tag();
                if response.status() < 200 {
                    if let Some(tag) = peer_tag {
                        view.peer_rang.insert(tag.to_string());
                    }
                    continue;
                }
                // A 2xx confirms the fork it rode; a non-2xx ends every early
                // dialog the peer opened for this INVITE (§12.3).
                if (200..300).contains(&response.status()) {
                    if let Some(tag) = peer_tag {
                        view.peer_rang.remove(tag);
                    }
                    view.held_dialog = view.own_tag.clone();
                } else {
                    view.peer_rang.clear();
                }
                view.took_final = Some(response);
            }
        }
    }
    view
}

fn parse(raw: &str) -> Option<SipMessage> {
    CustomParser::new().parse(raw.as_bytes()).ok()
}

/// The server transaction a taken request opens, as the dialog names it.
fn transaction_of(request: &SipRequest) -> Transaction {
    (request.cseq().seq(), request.to().tag().map(str::to_string))
}

/// The reliable provisional a PRACK's RAck names in the PRACK's own dialog,
/// where it carries a well-formed one naming an INVITE (RFC 3262 §7.2).
fn rack_of(request: &SipRequest) -> Option<Reliable> {
    let rack = request.optional().rack.as_ref().ok()?.as_ref()?;
    (*rack.method() == Method::Invite)
        .then(|| (request.to().tag().map(str::to_string), rack.rseq(), rack.seq()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &str = "INVITE sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>\r\n\
        Call-ID: call-a\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    fn response(status: u16, cseq: &str) -> String {
        format!(
            "SIP/2.0 {status} Whatever\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>;tag=b1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    fn request(method: &str, cseq: &str) -> String {
        format!(
            "{method} sip:alice@127.0.0.1:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-x\r\n\
             From: <sip:bob@example.test>;tag=b1\r\n\
             To: <sip:alice@example.test>;tag=a1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// The response this leg sends to a [`request`] the peer sent it: the
    /// request's own From and To (RFC 3261 §8.2.6.2).
    fn reply(status: u16, cseq: &str) -> String {
        format!(
            "SIP/2.0 {status} Whatever\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-x\r\n\
             From: <sip:bob@example.test>;tag=b1\r\n\
             To: <sip:alice@example.test>;tag=a1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// A request this leg TOOK, on the dialog `to_tag` names, with `extra`
    /// headers ahead of the CSeq.
    fn taken(method: &str, cseq: &str, to_tag: Option<&str>, extra: &str) -> String {
        let tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
        format!(
            "{method} sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>{tag}\r\n\
             Call-ID: call-a\r\n\
             {extra}CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// A reliable provisional this leg SENT under `to_tag` (RFC 3262 §7.1).
    fn rang(status: u16, to_tag: &str, rseq: u32) -> String {
        response(status, "1 INVITE")
            .replace("tag=b1", &format!("tag={to_tag}"))
            .replace("CSeq:", &format!("Require: 100rel\r\nRSeq: {rseq}\r\nCSeq:"))
    }

    /// The answer a taken request is owed, as the close states it.
    fn answer(cseq_method: &str, cseq: u32, to_tag: Option<&str>, status: u16) -> Owed {
        Owed::Answer {
            cseq_method: cseq_method.into(),
            cseq,
            to_tag: to_tag.map(str::to_string),
            status,
            early_tag: None,
        }
    }

    /// [`answer`] riding the early dialog `early_tag` names.
    fn answer_under(
        cseq_method: &str,
        cseq: u32,
        to_tag: Option<&str>,
        status: u16,
        early_tag: &str,
    ) -> Owed {
        Owed::Answer {
            cseq_method: cseq_method.into(),
            cseq,
            to_tag: to_tag.map(str::to_string),
            status,
            early_tag: Some(early_tag.to_string()),
        }
    }

    /// A ladder's entries: `(dir, raw)`, and whether the datagram is a byte-
    /// identical repeat of an earlier one the recording marks as such.
    fn record(entries: &[(Dir, String, bool)]) -> Recording {
        let recording = Recording::new();
        recording.declare("A");
        for (at, (dir, raw, repeat)) in entries.iter().enumerate() {
            if *repeat {
                recording.push_repeat("A", *dir, at as u64, raw.clone(), None, None);
            } else {
                recording.push("A", *dir, at as u64, raw.clone(), None, None);
            }
        }
        recording
    }

    /// A leg's ladder, as `(dir, raw)` pairs in wire order.
    fn leg(entries: &[(Dir, String)]) -> Owed {
        let entries: Vec<_> = entries.iter().map(|(d, r)| (*d, r.clone(), false)).collect();
        obligations(&record(&entries)).remove("A").expect("the leg was recorded")
    }

    /// The same ladder, as the recorded messages [`unscripted`] reads.
    fn ladder(entries: &[(Dir, String)]) -> Vec<RecordedMessage> {
        let entries: Vec<_> = entries.iter().map(|(d, r)| (*d, r.clone(), false)).collect();
        record(&entries).legs().remove("A").expect("the leg was recorded")
    }

    fn message(raw: &str) -> SipMessage {
        parse(raw).expect("the fixture parses")
    }

    #[test]
    fn a_leg_that_never_spoke_owes_nothing() {
        assert_eq!(leg(&[]), Owed::Nothing);
    }

    /// The caller's shape, step by step: an INVITE with nothing behind it may
    /// not be cancelled (§9.1), one with a provisional may, one answered 2xx is
    /// acknowledged and then closed, and a closed one is over.
    #[test]
    fn the_leg_that_opened_the_dialog_acks_then_byes_and_cancels_before_a_final() {
        let invite = (Dir::Out, INVITE.to_string());
        let alone = leg(std::slice::from_ref(&invite));
        assert_eq!(alone, Owed::AwaitFinal, "§9.1 cancels nothing that has drawn no response");

        let trying = (Dir::In, response(100, "1 INVITE"));
        assert_eq!(leg(&[invite.clone(), trying.clone()]), Owed::Cancel);

        let ok = (Dir::In, response(200, "1 INVITE"));
        let Owed::Ack(acked) = leg(&[invite.clone(), trying.clone(), ok.clone()]) else {
            panic!("a 2xx nothing acknowledged is an ACK owed")
        };
        assert_eq!(acked.status(), 200);

        let ack = (Dir::Out, request("ACK", "1 ACK"));
        assert_eq!(leg(&[invite.clone(), trying.clone(), ok.clone(), ack.clone()]), Owed::Bye);

        let bye = (Dir::Out, request("BYE", "2 BYE"));
        assert_eq!(leg(&[invite, trying, ok, ack, bye]), Owed::Nothing, "the dialog is over");
    }

    /// A re-INVITE in flight when the script ends closes in ORDER: the
    /// re-INVITE's own 2xx is acknowledged first — per its own CSeq, not the
    /// original's — and the BYE that ends the dialog follows.
    #[test]
    fn a_reinvited_dialog_acks_the_reinvites_final_before_its_bye() {
        let mut ladder = vec![
            (Dir::Out, INVITE.to_string()),
            (Dir::In, response(200, "1 INVITE")),
            (Dir::Out, request("ACK", "1 ACK")),
            (Dir::Out, INVITE.replace("CSeq: 1 INVITE", "CSeq: 2 INVITE")),
            (Dir::In, response(200, "2 INVITE")),
        ];
        let Owed::Ack(acked) = leg(&ladder) else {
            panic!("the re-INVITE's unacknowledged 2xx is an ACK owed")
        };
        assert_eq!(acked.cseq().seq(), 2, "the re-INVITE's final, not the original's");

        ladder.push((Dir::Out, request("ACK", "2 ACK")));
        assert_eq!(leg(&ladder), Owed::Bye, "acknowledged, the dialog still owes its BYE");
    }

    /// A non-2xx final is acknowledged too (§17.1.1.3), and closes nothing.
    #[test]
    fn a_refused_invite_is_acknowledged_and_then_holds_nothing_open() {
        let invite = (Dir::Out, INVITE.to_string());
        let busy = (Dir::In, response(486, "1 INVITE"));
        assert!(matches!(leg(&[invite.clone(), busy.clone()]), Owed::Ack(_)));
        let ack = (Dir::Out, request("ACK", "1 ACK"));
        assert_eq!(leg(&[invite, busy, ack]), Owed::Nothing, "no dialog to BYE");
    }

    /// A CANCEL already sent leaves nothing to do but take the final it draws —
    /// and then acknowledge it (§17.1.1.3). The leg is not DONE meanwhile: a
    /// close that walked away here would leave the INVITE transaction to
    /// retransmit its reject to Timer H.
    #[test]
    fn a_cancel_already_sent_waits_for_its_final_and_acknowledges_it() {
        let invite = (Dir::Out, INVITE.to_string());
        let ringing = (Dir::In, response(180, "1 INVITE"));
        let cancel = (Dir::Out, request("CANCEL", "1 CANCEL"));
        let owed = leg(&[invite.clone(), ringing.clone(), cancel.clone()]);
        assert_eq!(owed, Owed::AwaitFinal);
        assert!(owed.open(), "the 487 is still coming, and still owes an ACK");
        let terminated = (Dir::In, response(487, "1 INVITE"));
        assert!(matches!(leg(&[invite, ringing, cancel, terminated]), Owed::Ack(_)));
    }

    /// The callee's shape: the side that ANSWERED never starts the teardown, and
    /// the BYE that arrives is answered rather than crossed.
    #[test]
    fn the_leg_that_answered_the_dialog_waits_for_the_teardown_and_answers_it() {
        let invite = (Dir::In, INVITE.to_string());
        // An INVITE it has not answered is answered now: the script stopped, and
        // the platform is holding a transaction open on it.
        assert_eq!(leg(std::slice::from_ref(&invite)), answer("INVITE", 1, None, 480));

        let ok = (Dir::Out, response(200, "1 INVITE"));
        assert_eq!(leg(&[invite.clone(), ok.clone()]), Owed::AwaitTeardown);

        let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
        assert_eq!(leg(&[invite.clone(), ok.clone(), ack.clone()]), Owed::AwaitTeardown);

        let bye = (Dir::In, taken("BYE", "2 BYE", Some("b1"), ""));
        assert_eq!(
            leg(&[invite.clone(), ok.clone(), ack.clone(), bye.clone()]),
            answer("BYE", 2, Some("b1"), 200)
        );

        let answered = (Dir::Out, response(200, "2 BYE"));
        assert_eq!(leg(&[invite, ok, ack, bye, answered]), Owed::Nothing);
    }

    /// A CANCEL jumps the queue and takes the INVITE with it: 200 to the CANCEL
    /// first (§9.2), then the 487 the cancelled INVITE ends with.
    #[test]
    fn a_cancelled_invite_is_answered_487_and_the_cancel_itself_first() {
        let invite = (Dir::In, INVITE.to_string());
        let ringing = (Dir::Out, response(180, "1 INVITE"));
        let cancel = (Dir::In, taken("CANCEL", "1 CANCEL", None, ""));
        assert_eq!(
            leg(&[invite.clone(), ringing.clone(), cancel.clone()]),
            answer("CANCEL", 1, None, 200)
        );
        let answered = (Dir::Out, response(200, "1 CANCEL"));
        assert_eq!(leg(&[invite, ringing, cancel, answered]), answer("INVITE", 1, None, 487));
    }

    /// RFC 3262 §3, both halves: a PRACK whose RAck names the reliable
    /// provisional this leg sent and nothing has acknowledged draws a 200, and
    /// one naming anything else — another RSeq, a provisional a PRACK it
    /// answered already acknowledged, or no well-formed RAck at all — draws a
    /// 481. Once answered, the same PRACK owes nothing more.
    #[test]
    fn an_unscripted_prack_draws_200_for_the_provisional_it_names_and_481_otherwise() {
        let invite = (Dir::In, INVITE.to_string());
        let reliable = (Dir::Out, rang(180, "b1", 7));
        let prack_raw = taken("PRACK", "2 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n");
        let prack = (Dir::In, prack_raw.clone());
        let ok = answer("PRACK", 2, Some("b1"), 200);

        let named = ladder(&[invite.clone(), reliable.clone(), prack.clone()]);
        assert_eq!(unscripted(&named, &message(&prack_raw)), Some(ok.clone()));
        // The generic close answers it ahead of the older INVITE transaction:
        // the PRACK's client transaction is the one running a timer, and a
        // non-2xx final first would end the early dialog it rides (§12.3).
        assert_eq!(leg(&[invite.clone(), reliable.clone(), prack.clone()]), ok);

        let other_raw = prack_raw.replace("RAck: 7 1 INVITE", "RAck: 9 1 INVITE");
        let other = ladder(&[invite.clone(), reliable.clone(), (Dir::In, other_raw.clone())]);
        let refused = Some(answer("PRACK", 2, Some("b1"), 481));
        assert_eq!(unscripted(&other, &message(&other_raw)), refused);

        let bare_raw = taken("PRACK", "2 PRACK", Some("b1"), "");
        let bare = ladder(&[invite.clone(), reliable.clone(), (Dir::In, bare_raw.clone())]);
        assert_eq!(unscripted(&bare, &message(&bare_raw)), refused, "no RAck names nothing");

        let answered = (Dir::Out, response(200, "2 PRACK"));
        let done = ladder(&[invite.clone(), reliable.clone(), prack.clone(), answered.clone()]);
        assert_eq!(unscripted(&done, &message(&prack_raw)), None, "one final per transaction");
        let again_raw = taken("PRACK", "3 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n");
        let again = ladder(&[invite, reliable, prack, answered, (Dir::In, again_raw.clone())]);
        assert_eq!(
            unscripted(&again, &message(&again_raw)),
            Some(answer("PRACK", 3, Some("b1"), 481))
        );
    }

    /// The CANCEL pair is the CANCEL's own whatever else the leg holds open: a
    /// PRACK still unanswered does not stand between the 200 to the CANCEL and
    /// the 487 to the INVITE it names (§9.2).
    #[test]
    fn an_unscripted_cancel_still_draws_the_487_past_an_open_prack() {
        let cancel_raw = taken("CANCEL", "1 CANCEL", None, "");
        let ladder = ladder(&[
            (Dir::In, INVITE.to_string()),
            (Dir::Out, rang(180, "b1", 7)),
            (Dir::In, taken("PRACK", "2 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n")),
            (Dir::In, cancel_raw.clone()),
            (Dir::Out, response(200, "1 CANCEL")),
        ]);
        assert_eq!(
            unscripted(&ladder, &message(&cancel_raw)),
            Some(answer("INVITE", 1, None, 487))
        );
    }

    /// A byte-identical repeat of a request the leg already answered re-opens
    /// nothing: the server transaction is Completed and RFC 3261 §17.2.2 lets it
    /// re-send the last response or stay silent, never emit a second final.
    #[test]
    fn a_repeat_of_an_answered_request_reopens_no_transaction() {
        let prack = taken("PRACK", "2 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n");
        let recording = record(&[
            (Dir::In, INVITE.to_string(), false),
            (Dir::Out, rang(180, "b1", 7), false),
            (Dir::In, prack.clone(), false),
            (Dir::Out, response(200, "2 PRACK"), false),
            // The peer's retransmission crossed the 200 on the wire.
            (Dir::In, prack, true),
            (Dir::Out, response(486, "1 INVITE"), false),
        ]);
        assert_eq!(obligations(&recording).remove("A"), Some(Owed::Nothing));
    }

    /// RFC 3262 §3 matches a PRACK within the SAME early dialog as the
    /// provisional: two forks on one leg may each ring under `RSeq: 1` and be
    /// acknowledged by a PRACK numbered 2 in their own space, so the dialog's
    /// To-tag is part of the key — for the provisional, the PRACK, and the
    /// answer that closes the PRACK's transaction.
    #[test]
    fn a_prack_is_matched_within_its_own_early_dialog() {
        let invite = (Dir::In, INVITE.to_string());
        let fork1 = (Dir::Out, rang(183, "t1", 1));
        let fork2 = (Dir::Out, rang(180, "t2", 1));
        let prack1_raw = taken("PRACK", "2 PRACK", Some("t1"), "RAck: 1 1 INVITE\r\n");
        let prack2_raw = taken("PRACK", "2 PRACK", Some("t2"), "RAck: 1 1 INVITE\r\n");
        let prack1 = (Dir::In, prack1_raw.clone());
        let prack2 = (Dir::In, prack2_raw.clone());
        let ok1 = (Dir::Out, response(200, "2 PRACK").replace("tag=b1", "tag=t1"));

        // Fork 1's 200 closes fork 1's PRACK only: fork 2's is still owed its own.
        let both = ladder(&[invite.clone(), fork1.clone(), fork2.clone(), prack1, prack2, ok1]);
        assert_eq!(
            unscripted(&both, &message(&prack2_raw)),
            Some(answer("PRACK", 2, Some("t2"), 200))
        );
        // A PRACK carrying fork 1's RAck under fork 2's tag names no provisional
        // of fork 2's.
        let crossed_raw = prack1_raw.replace("tag=t1", "tag=t2").replace("RAck: 1", "RAck: 3");
        let crossed = ladder(&[invite, fork1, fork2, (Dir::In, crossed_raw.clone())]);
        assert_eq!(
            unscripted(&crossed, &message(&crossed_raw)),
            Some(answer("PRACK", 2, Some("t2"), 481))
        );
    }

    /// Two PRACKs open on one dialog are answered oldest first, and each with
    /// ITS OWN status: the obligation names the transaction it answers, not
    /// only the method.
    #[test]
    fn two_open_pracks_are_answered_in_order_each_with_its_own_status() {
        let invite = (Dir::In, INVITE.to_string());
        let reliable = (Dir::Out, rang(180, "b1", 7));
        let named = (Dir::In, taken("PRACK", "2 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n"));
        let stray = (Dir::In, taken("PRACK", "3 PRACK", Some("b1"), "RAck: 9 1 INVITE\r\n"));
        let mut ladder = vec![invite, reliable, named, stray];
        assert_eq!(leg(&ladder), answer("PRACK", 2, Some("b1"), 200));
        ladder.push((Dir::Out, response(200, "2 PRACK")));
        assert_eq!(leg(&ladder), answer("PRACK", 3, Some("b1"), 481));
    }

    /// A method the RFC gives no termination answer for is left alone: it holds
    /// no call up, and answering it would put a message on the wire nothing
    /// asked for.
    #[test]
    fn a_request_no_rule_answers_is_left_to_the_platform() {
        let options = (Dir::In, taken("OPTIONS", "9 OPTIONS", None, ""));
        assert_eq!(leg(&[options]), Owed::Nothing);
    }

    /// The act each obligation puts on the wire, and the ones that put none.
    #[test]
    fn only_an_obligation_with_an_emission_names_an_act() {
        assert_eq!(Owed::Nothing.act(), None);
        assert_eq!(Owed::AwaitFinal.act(), None);
        assert_eq!(Owed::AwaitTeardown.act(), None);
        assert_eq!(Owed::Bye.act(), Some(CloseOwed::Bye));
        assert_eq!(Owed::Cancel.act(), Some(CloseOwed::Cancel));
        assert_eq!(answer("BYE", 2, Some("b1"), 200).act(), Some(CloseOwed::Answer));
        // Waiting is still OPEN: the close is not finished while a teardown the
        // far side owes has not arrived.
        assert!(Owed::AwaitTeardown.open());
        assert!(!Owed::Nothing.open());
    }

    /// §9.2, both halves: the CANCEL is answered 200, and once that is on the
    /// recording the INVITE it named is answered 487.
    #[test]
    fn an_unscripted_cancel_draws_the_200_and_then_the_487() {
        let invite = (Dir::In, INVITE.to_string());
        let ringing = (Dir::Out, response(180, "1 INVITE"));
        let raw = taken("CANCEL", "1 CANCEL", None, "");
        let cancel = (Dir::In, raw.clone());
        let trigger = message(&raw);

        let first = ladder(&[invite.clone(), ringing.clone(), cancel.clone()]);
        assert_eq!(unscripted(&first, &trigger), Some(answer("CANCEL", 1, None, 200)));

        let answered = (Dir::Out, response(200, "1 CANCEL"));
        let second = ladder(&[invite.clone(), ringing.clone(), cancel.clone(), answered.clone()]);
        assert_eq!(unscripted(&second, &trigger), Some(answer("INVITE", 1, None, 487)));

        let terminated = (Dir::Out, response(487, "1 INVITE"));
        let third = ladder(&[invite, ringing, cancel, answered, terminated]);
        assert_eq!(unscripted(&third, &trigger), None, "the pair is discharged");
    }

    /// §17.1.1.3: a non-2xx final this leg took for the INVITE it sent is ACKed,
    /// and the ACK names that final's own transaction.
    #[test]
    fn an_unscripted_non_2xx_final_draws_an_ack_for_its_own_transaction() {
        let invite = (Dir::Out, INVITE.to_string());
        let raw = response(487, "1 INVITE");
        let terminated = (Dir::In, raw.clone());
        let trigger = message(&raw);

        let taken = ladder(&[invite.clone(), terminated.clone()]);
        let Some(Owed::Ack(acked)) = unscripted(&taken, &trigger) else {
            panic!("a non-2xx final nothing acknowledged is an ACK owed")
        };
        assert_eq!((acked.status(), acked.cseq().seq()), (487, 1));

        let ack = (Dir::Out, request("ACK", "1 ACK"));
        assert_eq!(
            unscripted(&ladder(&[invite, terminated, ack]), &trigger),
            None,
            "the obligation is discharged once the ACK is on the wire"
        );
    }

    /// A 2xx is NOT this seam's: RFC 3261 §13.2.2.4 leaves it to the dialog the
    /// flow itself scripts an ACK for.
    #[test]
    fn an_unscripted_2xx_final_draws_nothing_here() {
        let invite = (Dir::Out, INVITE.to_string());
        let raw = response(200, "1 INVITE");
        let ok = (Dir::In, raw.clone());
        assert_eq!(unscripted(&ladder(&[invite, ok]), &message(&raw)), None);
    }

    /// Every other unscripted arrival is refused and answered by nobody: no
    /// rule makes an INFO or an OPTIONS the transaction layer's own.
    #[test]
    fn an_unscripted_arrival_of_any_other_method_draws_no_invented_answer() {
        let invite = (Dir::In, INVITE.to_string());
        let ok = (Dir::Out, response(200, "1 INVITE"));
        for (method, tag) in [("OPTIONS", None), ("INFO", Some("b1"))] {
            let raw = taken(method, &format!("9 {method}"), tag, "");
            let arrival = (Dir::In, raw.clone());
            assert_eq!(
                unscripted(&ladder(&[invite.clone(), ok.clone(), arrival]), &message(&raw)),
                None,
                "{method} is nobody's transaction obligation"
            );
        }
    }

    /// RFC 3261 §15.1.2: a BYE on the dialog this leg holds is answered `200`,
    /// once, from whichever side of the dialog this leg is on — and while this
    /// leg's own BYE still awaits its final (§15.1.1). A BYE matching no
    /// dialog of this leg's — another tag, no tag, one an answered BYE already
    /// ended — draws the `481` the same section states.
    #[test]
    fn an_unscripted_bye_draws_200_on_the_held_dialog_and_481_on_none() {
        // The side that ANSWERED the dialog holds it under the tag its 2xx minted.
        let invite = (Dir::In, INVITE.to_string());
        let ok = (Dir::Out, response(200, "1 INVITE"));
        let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
        let bye_raw = taken("BYE", "2 BYE", Some("b1"), "");
        let bye = (Dir::In, bye_raw.clone());
        let held = ladder(&[invite.clone(), ok.clone(), ack.clone(), bye.clone()]);
        assert_eq!(unscripted(&held, &message(&bye_raw)), Some(answer("BYE", 2, Some("b1"), 200)));
        let answered = (Dir::Out, response(200, "2 BYE"));
        let done = ladder(&[invite.clone(), ok.clone(), ack.clone(), bye, answered.clone()]);
        assert_eq!(unscripted(&done, &message(&bye_raw)), None, "one final per transaction");

        let other_raw = taken("BYE", "2 BYE", Some("b2"), "");
        let other =
            ladder(&[invite.clone(), ok.clone(), ack.clone(), (Dir::In, other_raw.clone())]);
        assert_eq!(
            unscripted(&other, &message(&other_raw)),
            Some(answer("BYE", 2, Some("b2"), 481)),
            "another tag is no dialog of ours"
        );

        let bare_raw = taken("BYE", "2 BYE", None, "");
        let bare = ladder(&[invite.clone(), ok.clone(), ack.clone(), (Dir::In, bare_raw.clone())]);
        assert_eq!(
            unscripted(&bare, &message(&bare_raw)),
            Some(answer("BYE", 2, None, 481)),
            "no tag names no dialog"
        );

        let ringing = (Dir::Out, response(180, "1 INVITE"));
        let again_raw = taken("BYE", "3 BYE", Some("b1"), "");
        let again = ladder(&[
            invite.clone(),
            ringing,
            ok.clone(),
            ack.clone(),
            (Dir::In, bye_raw.clone()),
            answered,
            (Dir::In, again_raw.clone()),
        ]);
        assert_eq!(
            unscripted(&again, &message(&again_raw)),
            Some(answer("BYE", 3, Some("b1"), 481)),
            "the dialog it rang and confirmed under is over"
        );

        // A 200 to a BYE on another tag (the generic close answers every
        // BYE) ends only that tag's dialog: the held one is still ours.
        let stray = (Dir::In, taken("BYE", "3 BYE", Some("b9"), ""));
        let stray_ok = (Dir::Out, response(200, "3 BYE").replace("tag=b1", "tag=b9"));
        let still_held = ladder(&[invite, ok, ack, stray, stray_ok, (Dir::In, bye_raw.clone())]);
        assert_eq!(
            unscripted(&still_held, &message(&bye_raw)),
            Some(answer("BYE", 2, Some("b1"), 200)),
            "the held dialog outlives another tag's BYE"
        );

        // The side that OPENED the dialog holds it under its INVITE's From-tag.
        let sent = (Dir::Out, INVITE.to_string());
        let took = (Dir::In, response(200, "1 INVITE"));
        let acked = (Dir::Out, request("ACK", "1 ACK"));
        let peer_raw = request("BYE", "2 BYE");
        let peer = (Dir::In, peer_raw.clone());
        let opened = ladder(&[sent.clone(), took.clone(), acked.clone(), peer.clone()]);
        assert_eq!(
            unscripted(&opened, &message(&peer_raw)),
            Some(answer("BYE", 2, Some("a1"), 200))
        );

        // A BYE crossing ours: the dialog ends on OUR BYE's final (§15.1.1),
        // and until then the peer's names it.
        let ours = (Dir::Out, request("BYE", "2 BYE"));
        let crossed_raw = request("BYE", "3 BYE");
        let crossed = ladder(&[sent, took, acked, ours, (Dir::In, crossed_raw.clone())]);
        assert_eq!(
            unscripted(&crossed, &message(&crossed_raw)),
            Some(answer("BYE", 3, Some("a1"), 200)),
            "our BYE has no final yet"
        );
    }

    /// RFC 3261 §15: a BYE may end an EARLY dialog — one a tagged provisional
    /// opened (§12.1.1) and no final has confirmed or ended — and §15.1.2
    /// answers it like any dialog's: `200`, then the `487` the INVITE that
    /// dialog rides is still owed, under the tag the dialog rang with
    /// (§8.2.6.2). A tag that rang and then took a 2xx is the held dialog;
    /// one that rang and then took a non-2xx is gone (§12.3), so its BYE
    /// draws `481`. On the side that OPENED the dialog the early dialog is
    /// the peer's tagged provisional, and the BYE it sends on it draws the
    /// `200` too — the INVITE stays the peer's to answer, so no second act.
    #[test]
    fn an_unscripted_bye_on_an_early_dialog_draws_200_then_487s_the_invite_it_rides() {
        // The side that ANSWERED: ringing under b1, no final yet.
        let invite = (Dir::In, INVITE.to_string());
        let ringing = (Dir::Out, response(180, "1 INVITE"));
        let bye_raw = taken("BYE", "2 BYE", Some("b1"), "");
        let bye = (Dir::In, bye_raw.clone());
        let trigger = message(&bye_raw);
        let early = ladder(&[invite.clone(), ringing.clone(), bye.clone()]);
        assert_eq!(unscripted(&early, &trigger), Some(answer("BYE", 2, Some("b1"), 200)));

        let answered = (Dir::Out, response(200, "2 BYE"));
        let ended = ladder(&[invite.clone(), ringing.clone(), bye.clone(), answered.clone()]);
        let terminated = answer_under("INVITE", 1, None, 487, "b1");
        assert_eq!(
            unscripted(&ended, &trigger),
            Some(terminated.clone()),
            "the INVITE the early dialog rides, under the tag it rang with"
        );
        // The generic close reads the same fact: the pending INVITE owes the
        // 487 the ended dialog recommends, not the 480 of a script that stopped.
        assert_eq!(
            leg(&[invite.clone(), ringing.clone(), bye.clone(), answered.clone()]),
            terminated
        );

        let again_raw = taken("BYE", "3 BYE", Some("b1"), "");
        let again = ladder(&[
            invite.clone(),
            ringing.clone(),
            bye.clone(),
            answered.clone(),
            (Dir::In, again_raw.clone()),
        ]);
        assert_eq!(
            unscripted(&again, &message(&again_raw)),
            Some(answer("BYE", 3, Some("b1"), 481)),
            "the early dialog that BYE ended is gone"
        );

        let final_out = (Dir::Out, response(487, "1 INVITE"));
        let done = ladder(&[
            invite.clone(),
            ringing.clone(),
            bye.clone(),
            answered.clone(),
            final_out.clone(),
        ]);
        assert_eq!(unscripted(&done, &trigger), None, "the pair is discharged");

        // Two forks ringing: the 487 ends the INVITE transaction, and with it
        // EVERY early dialog it opened (§12.3) — the other fork's BYE is late.
        let second_fork = (Dir::Out, response(183, "1 INVITE").replace("tag=b1", "tag=b2"));
        let other_raw = taken("BYE", "3 BYE", Some("b2"), "");
        let forked = ladder(&[
            invite.clone(),
            ringing.clone(),
            second_fork,
            bye,
            answered,
            final_out,
            (Dir::In, other_raw.clone()),
        ]);
        assert_eq!(
            unscripted(&forked, &message(&other_raw)),
            Some(answer("BYE", 3, Some("b2"), 481)),
            "the non-2xx ended the other fork's early dialog too"
        );

        // Rang and then refused: the early dialog ended with the non-2xx.
        let refused = (Dir::Out, response(486, "1 INVITE"));
        let gone = ladder(&[invite.clone(), ringing.clone(), refused, (Dir::In, bye_raw.clone())]);
        assert_eq!(
            unscripted(&gone, &trigger),
            Some(answer("BYE", 2, Some("b1"), 481)),
            "the dialog it rang under is over"
        );

        // Rang and then confirmed: the held dialog, under the same tag.
        let ok = (Dir::Out, response(200, "1 INVITE"));
        let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
        let held = ladder(&[invite, ringing, ok, ack, (Dir::In, bye_raw)]);
        assert_eq!(unscripted(&held, &trigger), Some(answer("BYE", 2, Some("b1"), 200)));

        // The side that OPENED: the peer rang under b1, and BYEs that dialog.
        let sent = (Dir::Out, INVITE.to_string());
        let took = (Dir::In, response(180, "1 INVITE"));
        let peer_raw = request("BYE", "2 BYE");
        let peer = (Dir::In, peer_raw.clone());
        let opened = ladder(&[sent.clone(), took.clone(), peer.clone()]);
        assert_eq!(
            unscripted(&opened, &message(&peer_raw)),
            Some(answer("BYE", 2, Some("a1"), 200))
        );
        let peer_answered = (Dir::Out, reply(200, "2 BYE"));
        let peer_ended = ladder(&[sent.clone(), took.clone(), peer.clone(), peer_answered.clone()]);
        assert_eq!(
            unscripted(&peer_ended, &message(&peer_raw)),
            None,
            "the INVITE this leg sent is the peer's to answer"
        );
        let peer_again_raw = request("BYE", "3 BYE");
        let peer_again = ladder(&[
            sent.clone(),
            took.clone(),
            peer,
            peer_answered,
            (Dir::In, peer_again_raw.clone()),
        ]);
        assert_eq!(
            unscripted(&peer_again, &message(&peer_again_raw)),
            Some(answer("BYE", 3, Some("a1"), 481)),
            "the early dialog that BYE ended is gone"
        );

        // A BYE from a tag the peer never rang under names no early dialog.
        let stranger_raw = peer_raw.replace("tag=b1", "tag=b2");
        let stranger = ladder(&[sent.clone(), took.clone(), (Dir::In, stranger_raw.clone())]);
        assert_eq!(
            unscripted(&stranger, &message(&stranger_raw)),
            Some(answer("BYE", 2, Some("a1"), 481)),
            "no early dialog under that tag"
        );

        // The peer's non-2xx ended its early dialog before the BYE.
        let peer_refused = (Dir::In, response(486, "1 INVITE"));
        let acked = (Dir::Out, request("ACK", "1 ACK"));
        let peer_gone = ladder(&[
            sent.clone(),
            took.clone(),
            peer_refused.clone(),
            acked.clone(),
            (Dir::In, peer_raw.clone()),
        ]);
        assert_eq!(
            unscripted(&peer_gone, &message(&peer_raw)),
            Some(answer("BYE", 2, Some("a1"), 481)),
            "the early dialog ended with the 486"
        );
        // The peer rang two forks: the 486 under one ends both early dialogs
        // (§12.3), so the other fork's BYE names no dialog either.
        let took_other = (Dir::In, response(183, "1 INVITE").replace("tag=b1", "tag=b2"));
        let forked_gone =
            ladder(&[sent, took, took_other, peer_refused, acked, (Dir::In, stranger_raw.clone())]);
        assert_eq!(
            unscripted(&forked_gone, &message(&stranger_raw)),
            Some(answer("BYE", 2, Some("a1"), 481)),
            "the non-2xx ended the other fork's early dialog too"
        );
    }

    /// RFC 3261 §15.1.2, last paragraph: the UAS that answered a BYE still
    /// responds to every request pending on that dialog, 487 recommended. A
    /// re-INVITE the flow scripted nothing for is open on the leg when the
    /// BYE lands: the BYE draws its 200 first, then the INVITE its 487, and a
    /// third call owes nothing. A BYE answered 481 ended no dialog, so the
    /// INVITE it left open stays the platform's to answer.
    #[test]
    fn an_unscripted_bye_answered_200_then_487s_the_re_invite_pending_on_that_dialog() {
        let invite = (Dir::In, INVITE.to_string());
        let ok = (Dir::Out, response(200, "1 INVITE"));
        let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
        let reinvite = (Dir::In, taken("INVITE", "2 INVITE", Some("b1"), ""));
        let bye_raw = taken("BYE", "3 BYE", Some("b1"), "");
        let bye = (Dir::In, bye_raw.clone());
        let trigger = message(&bye_raw);

        let first =
            ladder(&[invite.clone(), ok.clone(), ack.clone(), reinvite.clone(), bye.clone()]);
        assert_eq!(unscripted(&first, &trigger), Some(answer("BYE", 3, Some("b1"), 200)));

        let answered = (Dir::Out, response(200, "3 BYE"));
        let second = ladder(&[
            invite.clone(),
            ok.clone(),
            ack.clone(),
            reinvite.clone(),
            bye.clone(),
            answered.clone(),
        ]);
        assert_eq!(
            unscripted(&second, &trigger),
            Some(answer("INVITE", 2, Some("b1"), 487)),
            "the re-INVITE pending on the ended dialog"
        );
        assert_eq!(
            leg(&[
                invite.clone(),
                ok.clone(),
                ack.clone(),
                reinvite.clone(),
                bye.clone(),
                answered.clone()
            ]),
            answer("INVITE", 2, Some("b1"), 487),
            "the generic close reads the ended dialog too"
        );

        let terminated = (Dir::Out, response(487, "2 INVITE"));
        let third = ladder(&[
            invite.clone(),
            ok.clone(),
            ack.clone(),
            reinvite,
            bye.clone(),
            answered.clone(),
            terminated,
        ]);
        assert_eq!(unscripted(&third, &trigger), None, "the pair is discharged");

        // A BYE on no dialog of ours ends nothing: its 481 leaves the INVITE alone.
        let stray_raw = taken("BYE", "3 BYE", Some("b2"), "");
        let stray_reinvite = (Dir::In, taken("INVITE", "2 INVITE", Some("b2"), ""));
        let refused = (Dir::Out, response(481, "3 BYE").replace("tag=b1", "tag=b2"));
        let stray = ladder(&[
            invite.clone(),
            ok.clone(),
            ack.clone(),
            stray_reinvite.clone(),
            (Dir::In, stray_raw.clone()),
            refused,
        ]);
        assert_eq!(unscripted(&stray, &message(&stray_raw)), None, "no dialog of ours ended");

        // The 200 ended OUR dialog; an INVITE pending under another tag is on
        // another dialog and is not this BYE's to answer.
        let other = ladder(&[invite, ok, ack, stray_reinvite, bye, answered]);
        assert_eq!(unscripted(&other, &trigger), None, "a pending INVITE on another dialog");
    }

    /// A leg holding an unanswered request that is NOT the cancelled INVITE
    /// answers nothing for it: the CANCEL's own pair is the whole scope.
    #[test]
    fn a_cancel_answers_its_own_pair_and_no_other_request_the_leg_holds() {
        let options = (Dir::In, taken("OPTIONS", "9 OPTIONS", None, ""));
        let raw = taken("CANCEL", "1 CANCEL", None, "");
        let cancel = (Dir::In, raw.clone());
        let trigger = message(&raw);
        let answered = (Dir::Out, response(200, "1 CANCEL"));
        assert_eq!(
            unscripted(&ladder(&[options.clone(), cancel.clone()]), &trigger),
            Some(answer("CANCEL", 1, None, 200))
        );
        assert_eq!(
            unscripted(&ladder(&[options, cancel, answered]), &trigger),
            None,
            "the OPTIONS this leg holds is not the CANCEL's to answer"
        );
    }
}
