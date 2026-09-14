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
use sip_message::{HeaderName, Method, SipMessage, SipParser, SipRequest, SipResponse};

use crate::recording::Recording;

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
    /// A request this leg took and never answered with a final.
    Answer { cseq_method: String, status: u16 },
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
            Owed::Answer { cseq_method, status } => {
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
    /// Whether this leg TOOK an INVITE, and the final it answered it with.
    took_invite: bool,
    answered_invite: Option<u16>,
    /// Whether a CANCEL arrived for the INVITE this leg took.
    took_cancel: bool,
    /// Requests taken and not answered with a final, oldest first.
    unanswered: Vec<(Method, u32)>,
    /// The reliable provisionals this leg SENT, as `(RSeq, INVITE CSeq)`, and
    /// the ones a PRACK it answered 2xx has acknowledged (RFC 3262 §3).
    sent_reliable: BTreeSet<(u32, u32)>,
    acknowledged: BTreeSet<(u32, u32)>,
    /// The RAck of every PRACK this leg took, by the PRACK's own CSeq.
    took_prack: BTreeMap<u32, Option<(u32, u32)>>,
}

/// What one leg owes, read off its recorded ladder.
fn owed(messages: &[RecordedMessage]) -> Owed {
    let view = view(messages);

    // A request the peer is still waiting on comes first: an unanswered server
    // transaction retransmits, and every other obligation is behind it.
    if let Some((method, status)) = view.to_answer() {
        return Owed::Answer { cseq_method: method.as_str().to_string(), status };
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

impl LegView {
    /// The request this leg answers first, and the status its method's own
    /// termination needs.
    ///
    /// A CANCEL jumps the queue (RFC 3261 §9.2 answers it at once and the INVITE
    /// it names becomes a 487); otherwise the oldest unanswered request goes
    /// first, because that is the one the peer has been retransmitting longest.
    fn to_answer(&self) -> Option<(Method, u16)> {
        let cancel = self.unanswered.iter().find(|(method, _)| *method == Method::Cancel);
        let first = cancel.or_else(|| {
            self.unanswered.iter().find(|(method, _)| {
                *method == Method::Prack || answer_status(method, false).is_some()
            })
        })?;
        let (method, cseq) = first;
        let status = match method {
            Method::Prack => self.prack_status(*cseq),
            _ => answer_status(method, self.took_cancel)?,
        };
        Some((method.clone(), status))
    }

    /// The final a PRACK this leg took is answered with (RFC 3262 §3): 2xx when
    /// its RAck names a reliable provisional this leg sent and no PRACK has
    /// acknowledged yet, 481 otherwise.
    fn prack_status(&self, cseq: u32) -> u16 {
        match self.took_prack.get(&cseq).copied().flatten() {
            Some(rack)
                if self.sent_reliable.contains(&rack) && !self.acknowledged.contains(&rack) =>
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

/// What one leg owes for a datagram the flow scripts NO step for.
///
/// The refusal stands: this names only the transaction-layer acts the RFCs make
/// the endpoint's own whatever a document scripts — the `200` a CANCEL is
/// answered with and the `487` the INVITE it names ends with (RFC 3261 §9.2),
/// the ACK a non-2xx INVITE final is owed on its own branch (§17.1.1.3), and
/// the final a PRACK draws (RFC 3262 §3). Anything else owes nothing: answering
/// it would put a message on the wire nothing asked for.
///
/// One act per call, off the same ladder the generic close reads — so the
/// CANCEL pair is two calls, the second made once the first is on the recording.
pub fn unscripted(messages: &[RecordedMessage], trigger: &SipMessage) -> Option<Owed> {
    let view = view(messages);
    match trigger {
        SipMessage::Request(request) if *request.method() == Method::Cancel => {
            let (method, status) = view.to_answer()?;
            // Only the pair §9.2 makes this CANCEL's own: the CANCEL itself, and
            // the INVITE it names.
            matches!(&method, Method::Cancel | Method::Invite)
                .then(|| Owed::Answer { cseq_method: method.as_str().to_string(), status })
        }
        // RFC 3262 §3: a UAS answers every PRACK — 2xx for the unacknowledged
        // reliable provisional its RAck names, 481 for anything else — so the
        // relay it rode gets its own final and the caller's PRACK is not left
        // to time out on this leg's silence.
        SipMessage::Request(request) if *request.method() == Method::Prack => {
            let cseq = request.cseq().seq();
            let open = view.unanswered.iter().any(|(m, seq)| *m == Method::Prack && *seq == cseq);
            open.then(|| Owed::Answer {
                cseq_method: Method::Prack.as_str().to_string(),
                status: view.prack_status(cseq),
            })
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
        let Some(message) = parse(&recorded.raw) else { continue };
        match (recorded.dir, message) {
            (Dir::Out, SipMessage::Request(request)) => {
                let cseq = request.cseq().seq();
                match request.method() {
                    Method::Invite => view.sent_invite = Some(cseq),
                    Method::Ack => {
                        view.acked.insert(cseq);
                    }
                    Method::Cancel => view.sent_cancel = true,
                    Method::Bye => view.bye_seen = true,
                    _ => {}
                }
            }
            (Dir::In, SipMessage::Request(request)) => {
                let cseq = request.cseq().seq();
                match request.method() {
                    Method::Invite => view.took_invite = true,
                    Method::Cancel => view.took_cancel = true,
                    Method::Bye => view.bye_seen = true,
                    Method::Prack => {
                        view.took_prack.insert(cseq, rack_of(&request));
                    }
                    _ => {}
                }
                // An ACK answers no transaction of its own, so it is never owed
                // a response; everything else the leg took may be.
                if request.method() != Method::Ack {
                    view.unanswered.push((request.method().clone(), cseq));
                }
            }
            (Dir::Out, SipMessage::Response(response)) => {
                let cseq = response.cseq();
                if response.status() < 200 {
                    if cseq.method() == Method::Invite {
                        if let Some(rseq) = rseq_of(&response) {
                            view.sent_reliable.insert((rseq, cseq.seq()));
                        }
                    }
                    continue;
                }
                view.unanswered
                    .retain(|(method, seq)| !(method == cseq.method() && *seq == cseq.seq()));
                if cseq.method() == Method::Invite {
                    view.answered_invite = Some(response.status());
                }
                if cseq.method() == Method::Prack && (200..300).contains(&response.status()) {
                    if let Some(rack) = view.took_prack.get(&cseq.seq()).copied().flatten() {
                        view.acknowledged.insert(rack);
                    }
                }
            }
            (Dir::In, SipMessage::Response(response)) => {
                let cseq = response.cseq();
                if cseq.method() != Method::Invite || Some(cseq.seq()) != view.sent_invite {
                    continue;
                }
                view.heard_response = true;
                if response.status() >= 200 {
                    view.took_final = Some(response);
                }
            }
        }
    }
    view
}

fn parse(raw: &str) -> Option<SipMessage> {
    CustomParser::new().parse(raw.as_bytes()).ok()
}

/// The RSeq a reliable provisional carries (RFC 3262 §7.1), where it does.
fn rseq_of(response: &SipResponse) -> Option<u32> {
    response.raw(HeaderName::RSeq).next().and_then(|v| v.trim().parse().ok())
}

/// The `(RSeq, INVITE CSeq)` a PRACK's RAck names, where it carries a
/// well-formed one naming an INVITE (RFC 3262 §7.2).
fn rack_of(request: &SipRequest) -> Option<(u32, u32)> {
    let rack = request.optional().rack.as_ref().ok()?.as_ref()?;
    (*rack.method() == Method::Invite).then(|| (rack.rseq(), rack.seq()))
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

    /// A leg's ladder, as `(dir, raw)` pairs in wire order.
    fn leg(entries: &[(Dir, String)]) -> Owed {
        let recording = Recording::new();
        recording.declare("A");
        for (at, (dir, raw)) in entries.iter().enumerate() {
            recording.push("A", *dir, at as u64, raw.clone(), None, None);
        }
        obligations(&recording).remove("A").expect("the leg was recorded")
    }

    /// The same ladder, as the recorded messages [`unscripted`] reads.
    fn ladder(entries: &[(Dir, String)]) -> Vec<RecordedMessage> {
        let recording = Recording::new();
        recording.declare("A");
        for (at, (dir, raw)) in entries.iter().enumerate() {
            recording.push("A", *dir, at as u64, raw.clone(), None, None);
        }
        recording.legs().remove("A").expect("the leg was recorded")
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
        assert_eq!(
            leg(std::slice::from_ref(&invite)),
            Owed::Answer { cseq_method: "INVITE".into(), status: 480 }
        );

        let ok = (Dir::Out, response(200, "1 INVITE"));
        assert_eq!(leg(&[invite.clone(), ok.clone()]), Owed::AwaitTeardown);

        let ack = (Dir::In, request("ACK", "1 ACK"));
        assert_eq!(leg(&[invite.clone(), ok.clone(), ack.clone()]), Owed::AwaitTeardown);

        let bye = (Dir::In, request("BYE", "2 BYE"));
        assert_eq!(
            leg(&[invite.clone(), ok.clone(), ack.clone(), bye.clone()]),
            Owed::Answer { cseq_method: "BYE".into(), status: 200 }
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
        let cancel = (Dir::In, request("CANCEL", "1 CANCEL"));
        assert_eq!(
            leg(&[invite.clone(), ringing.clone(), cancel.clone()]),
            Owed::Answer { cseq_method: "CANCEL".into(), status: 200 }
        );
        let answered = (Dir::Out, response(200, "1 CANCEL"));
        assert_eq!(
            leg(&[invite, ringing, cancel, answered]),
            Owed::Answer { cseq_method: "INVITE".into(), status: 487 }
        );
    }

    /// RFC 3262 §3, both halves: a PRACK whose RAck names the reliable
    /// provisional this leg sent and nothing has acknowledged draws a 200, and
    /// one naming anything else — another RSeq, or a provisional a PRACK it
    /// answered already acknowledged — draws a 481.
    #[test]
    fn an_unscripted_prack_draws_200_for_the_provisional_it_names_and_481_otherwise() {
        let invite = (Dir::In, INVITE.to_string());
        let reliable = (Dir::Out, response(180, "1 INVITE").replace("CSeq:", "RSeq: 7\r\nCSeq:"));
        let prack_raw = request("PRACK", "2 PRACK").replace("CSeq:", "RAck: 7 1 INVITE\r\nCSeq:");
        let prack = (Dir::In, prack_raw.clone());
        let ok = Owed::Answer { cseq_method: "PRACK".into(), status: 200 };
        let refused = Some(Owed::Answer { cseq_method: "PRACK".into(), status: 481 });

        let named = ladder(&[invite.clone(), reliable.clone(), prack.clone()]);
        assert_eq!(unscripted(&named, &message(&prack_raw)), Some(ok.clone()));
        // The generic close answers the same PRACK once the older INVITE
        // transaction is ended (§3 lets the final precede the PRACK's answer).
        let ended = (Dir::Out, response(480, "1 INVITE"));
        assert_eq!(leg(&[invite.clone(), reliable.clone(), prack.clone(), ended]), ok);

        let other_raw = prack_raw.replace("RAck: 7 1 INVITE", "RAck: 9 1 INVITE");
        let other = ladder(&[invite.clone(), reliable.clone(), (Dir::In, other_raw.clone())]);
        assert_eq!(unscripted(&other, &message(&other_raw)), refused);

        let answered = (Dir::Out, response(200, "2 PRACK"));
        let done = ladder(&[invite.clone(), reliable.clone(), prack.clone(), answered.clone()]);
        assert_eq!(unscripted(&done, &message(&prack_raw)), None, "one final per transaction");
        let again_raw = prack_raw.replace("CSeq: 2 PRACK", "CSeq: 3 PRACK");
        let again = ladder(&[invite, reliable, prack, answered, (Dir::In, again_raw.clone())]);
        assert_eq!(unscripted(&again, &message(&again_raw)), refused);
    }

    /// A method the RFC gives no termination answer for is left alone: it holds
    /// no call up, and answering it would put a message on the wire nothing
    /// asked for.
    #[test]
    fn a_request_no_rule_answers_is_left_to_the_platform() {
        let options = (Dir::In, request("OPTIONS", "9 OPTIONS"));
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
        assert_eq!(
            Owed::Answer { cseq_method: "BYE".into(), status: 200 }.act(),
            Some(CloseOwed::Answer)
        );
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
        let raw = request("CANCEL", "1 CANCEL");
        let cancel = (Dir::In, raw.clone());
        let trigger = message(&raw);

        let first = ladder(&[invite.clone(), ringing.clone(), cancel.clone()]);
        assert_eq!(
            unscripted(&first, &trigger),
            Some(Owed::Answer { cseq_method: "CANCEL".into(), status: 200 })
        );

        let answered = (Dir::Out, response(200, "1 CANCEL"));
        let second = ladder(&[invite.clone(), ringing.clone(), cancel.clone(), answered.clone()]);
        assert_eq!(
            unscripted(&second, &trigger),
            Some(Owed::Answer { cseq_method: "INVITE".into(), status: 487 })
        );

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
    /// rule makes an INFO, an OPTIONS or a BYE the transaction layer's own.
    #[test]
    fn an_unscripted_arrival_of_any_other_method_draws_no_invented_answer() {
        let invite = (Dir::In, INVITE.to_string());
        let ok = (Dir::Out, response(200, "1 INVITE"));
        for method in ["OPTIONS", "INFO", "BYE"] {
            let raw = request(method, &format!("9 {method}"));
            let arrival = (Dir::In, raw.clone());
            assert_eq!(
                unscripted(&ladder(&[invite.clone(), ok.clone(), arrival]), &message(&raw)),
                None,
                "{method} is nobody's transaction obligation"
            );
        }
    }

    /// A leg holding an unanswered request that is NOT the cancelled INVITE
    /// answers nothing for it: the CANCEL's own pair is the whole scope.
    #[test]
    fn a_cancel_answers_its_own_pair_and_no_other_request_the_leg_holds() {
        let options = (Dir::In, request("OPTIONS", "9 OPTIONS"));
        let raw = request("CANCEL", "1 CANCEL");
        let cancel = (Dir::In, raw.clone());
        let trigger = message(&raw);
        let answered = (Dir::Out, response(200, "1 CANCEL"));
        assert_eq!(
            unscripted(&ladder(&[options.clone(), cancel.clone()]), &trigger),
            Some(Owed::Answer { cseq_method: "CANCEL".into(), status: 200 })
        );
        assert_eq!(
            unscripted(&ladder(&[options, cancel, answered]), &trigger),
            None,
            "the OPTIONS this leg holds is not the CANCEL's to answer"
        );
    }
}
