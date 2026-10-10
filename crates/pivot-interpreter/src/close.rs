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

/// A non-2xx final this leg SENT to an INVITE it took and the peer has not
/// acknowledged: the server transaction is Completed (RFC 3261 §17.2.1) until
/// the ACK the peer owes on the INVITE's branch (§17.1.1.3) or Timer H, which
/// runs from `sent_at_us`, the final's first emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnackedFinal {
    pub cseq: u32,
    pub status: u16,
    pub sent_at_us: u64,
}

/// The non-2xx INVITE finals one leg sent and is still owed an ACK for, by
/// CSeq, read off its recorded ladder. A 2xx is not here: its ACK is the
/// peer's own request (§13.2.2.4), not the transaction's closer. Nothing else
/// the leg answers is owed an ACK, so a non-INVITE final never appears.
pub fn unacked_finals(messages: &[RecordedMessage]) -> Vec<UnackedFinal> {
    view(messages)
        .sent_finals
        .into_iter()
        .map(|(cseq, (status, sent_at_us))| UnackedFinal { cseq, status, sent_at_us })
        .collect()
}

/// The recording note on a request a leg refused as naming no dialog it holds
/// (RFC 3261 §12.2.2).
pub(crate) const FOREIGN_DIALOG: &str =
    "names no dialog this leg holds (RFC 3261 §12.2.2): refused 481, nothing learned";

/// The recording note on the 481 that refuses it.
pub(crate) const FOREIGN_DIALOG_481: &str = "481: the request names no dialog this leg holds";

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
    /// The tag of the last dialog this leg held established, kept once the
    /// dialog is over: what a BYE this leg would send now names.
    last_held: Option<String>,
    /// The dialogs, by the tag this leg holds them under, on which it sent a
    /// REFER or a SUBSCRIBE: the subscriptions whose NOTIFYs it answers (RFC
    /// 6665 §4.1.3, RFC 3515 §2.4.4).
    subscribed: BTreeSet<Option<String>>,
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
    /// The non-2xx finals this leg SENT to INVITEs it took and the peer has
    /// not ACKed, by CSeq: the status and the instant of the first emission.
    /// Each holds a server transaction in Completed (RFC 3261 §17.2.1); the
    /// peer's ACK on that CSeq ends it (§17.1.1.3).
    sent_finals: BTreeMap<u32, (u16, u64)>,
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
/// final a PRACK draws (RFC 3262 §3), the `200` a NOTIFY of a subscription
/// this leg holds draws (RFC 6665 §4.1.3), and the final a BYE draws — `200` on
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
        // RFC 6665 §4.1.3: a subscriber answers every NOTIFY of a subscription
        // it holds 200 — the implicit one a REFER it sent created (RFC 3515
        // §2.4.4) included — so the notifier's transaction ends instead of
        // retransmitting to its Timer F. A NOTIFY for no subscription of this
        // leg's is left to the flow.
        SipMessage::Request(request) if *request.method() == Method::Notify => {
            let transaction = transaction_of(request);
            if !view.subscribed.contains(&transaction.1) {
                return None;
            }
            let open = view.unanswered.iter().find(|open| open.transaction == transaction)?;
            Some(open.answer(200))
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

/// Whether a BYE this leg would send now names a dialog it already ended by
/// answering the peer's BYE 2xx (RFC 3261 §15.1.2): the dialog is gone, so the
/// BYE would draw a 481 and end nothing.
pub fn bye_moot(messages: &[RecordedMessage]) -> bool {
    let view = view(messages);
    view.held_dialog.is_none() && view.last_held.is_some() && view.ended.contains(&view.last_held)
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
        // A request refused as naming no dialog the leg holds, and its 481,
        // are no part of the leg's dialog state (RFC 3261 §12.2.2).
        if matches!(recorded.note.as_deref(), Some(FOREIGN_DIALOG | FOREIGN_DIALOG_481)) {
            continue;
        }
        let Some(message) = parse(recorded.wire()) else { continue };
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
                    Method::Refer | Method::Subscribe => {
                        view.subscribed.insert(request.from().tag().map(str::to_string));
                    }
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
                    Method::Ack => {
                        view.sent_finals.remove(&request.cseq().seq());
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
                let ends_open = view.unanswered.iter().any(&ended);
                view.unanswered.retain(|open| !ended(open));
                match cseq.method() {
                    // A 2xx confirms the fork it rode; a non-2xx ends the
                    // INVITE transaction and every early dialog it opened
                    // (§12.3), and is owed the peer's ACK — a later final on
                    // a transaction already ended answers nothing and is
                    // owed nothing.
                    Method::Invite => {
                        view.answered_invite = Some(response.status());
                        if (200..300).contains(&response.status()) {
                            if let Some(tag) = &to_tag {
                                view.rang.remove(tag);
                            }
                            view.last_held.clone_from(&to_tag);
                            view.held_dialog = to_tag;
                        } else {
                            view.rang.retain(|_, invite| *invite != cseq.seq());
                            if ends_open {
                                view.sent_finals
                                    .insert(cseq.seq(), (response.status(), recorded.at_us));
                            }
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
                    view.last_held = view.own_tag.clone();
                } else {
                    view.peer_rang.clear();
                }
                view.took_final = Some(response);
            }
        }
    }
    view
}

fn parse(wire: &[u8]) -> Option<SipMessage> {
    CustomParser::new().parse(wire).ok()
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
mod tests;
