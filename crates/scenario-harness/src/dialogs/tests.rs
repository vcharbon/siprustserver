//! The dialog table's verdicts, fed message by message.

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse};

use super::*;

const CALL: &str = "c1@host";

fn parse(text: String) -> SipMessage {
    CustomParser::new().parse(text.as_bytes()).expect("a well-formed test message")
}

fn tagged(name: &str, tag: Option<&str>) -> String {
    match tag {
        Some(t) => format!("<sip:{name}@host>;tag={t}"),
        None => format!("<sip:{name}@host>"),
    }
}

/// A request from `from_tag` to `to_tag`, numbered `cseq`.
fn request(method: &str, cseq: u32, from_tag: &str, to_tag: Option<&str>) -> SipRequest {
    request_through(method, cseq, from_tag, to_tag, 0)
}

/// [`request`] as it arrives through `proxies` hops, each stacking its Via on
/// the sender's.
fn request_through(
    method: &str,
    cseq: u32,
    from_tag: &str,
    to_tag: Option<&str>,
    proxies: usize,
) -> SipRequest {
    let hops: String = (1..=proxies)
        .rev()
        .map(|n| {
            format!("Via: SIP/2.0/UDP 192.0.2.{n}:5060;branch=z9hG4bK-p{n}-{cseq}{method}\r\n")
        })
        .collect();
    let text = format!(
        "{method} sip:x@host SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n{hops}Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}{method}\r\n\
         From: {}\r\nTo: {}\r\nCall-ID: {CALL}\r\nCSeq: {cseq} {method}\r\n\
         Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n",
        tagged("a", Some(from_tag)),
        tagged("b", to_tag),
    );
    match parse(text) {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => unreachable!(),
    }
}

/// A `status` response to `method` numbered `cseq`, From `from_tag`, To `to_tag`.
fn response(status: u16, method: &str, cseq: u32, from_tag: &str, to_tag: &str) -> SipResponse {
    let text = format!(
        "SIP/2.0 {status} Reason\r\nContact: <sip:fixture@192.0.2.98>\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}{method}\r\n\
         From: {}\r\nTo: {}\r\nCall-ID: {CALL}\r\nCSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n",
        tagged("a", Some(from_tag)),
        tagged("b", Some(to_tag)),
    );
    match parse(text) {
        SipMessage::Response(r) => r,
        SipMessage::Request(_) => unreachable!(),
    }
}

/// A UAS (local tag `B1`) that took the caller `A1`'s INVITE (CSeq 1) and rang.
fn ringing_uas(backwards: BackwardsCseq) -> DialogTable {
    let mut t = DialogTable::new(backwards);
    assert_eq!(t.judge_received(&request("INVITE", 1, "A1", None), 0), None);
    t.note_sent_response(&response(180, "INVITE", 1, "A1", "B1"), 0);
    t
}

fn kind(v: Option<DialogVerdict>) -> Option<VerdictKind> {
    v.map(|v| v.kind)
}

/// As UAS the remote sequence starts at the INVITE's CSeq: the PRACK at 2 is
/// in order, a request under a remote tag the UA does not hold names no
/// dialog, and the message names the tags it holds.
#[test]
fn an_unknown_remote_tag_names_no_dialog() {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    assert_eq!(t.judge_received(&request("PRACK", 2, "A1", Some("B1")), 0), None);
    let v = t.judge_received(&request("OPTIONS", 3, "A9", Some("B1")), 0).expect("a verdict");
    assert_eq!(v.rule(), DIALOG_UNKNOWN);
    assert_eq!(v.to_string(), "OPTIONS From-tag A9 names no dialog (holds A1)");
    let v = t.judge_received(&request("OPTIONS", 3, "A1", Some("B7")), 0).expect("a verdict");
    assert_eq!(v.to_string(), "OPTIONS To-tag B7 names no dialog (holds B1)");
}

/// RFC 3261 §12.2.2: a CSeq below the last remote one goes backwards, one
/// more than one above it skips; a retransmission is judged once.
#[test]
fn cseq_backwards_and_skip() {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.note_sent_response(&response(200, "INVITE", 1, "A1", "B1"), 0);
    assert_eq!(t.judge_received(&request("INFO", 2, "A1", Some("B1")), 0), None);
    assert_eq!(t.judge_received(&request("INFO", 2, "A1", Some("B1")), 0), None, "a repeat");
    let v = t.judge_received(&request("INFO", 5, "A1", Some("B1")), 0).expect("a skip");
    assert_eq!(v.kind, VerdictKind::CseqSkip { cseq: 5, last: 2 });
    assert_eq!(v.to_string(), "INFO CSeq 5 skips on From-tag A1 (last remote CSeq 2)");
    let v = t.judge_received(&request("OPTIONS", 3, "A1", Some("B1")), 0).expect("backwards");
    assert_eq!(v.kind, VerdictKind::CseqBackwards { cseq: 3, last: 5 });
    assert_eq!(v.rule(), DIALOG_CSEQ_BACKWARDS);
    assert_eq!(v.kind.refusal(), Some((500, "Server Internal Error")));
    assert_eq!(t.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None, "ACK unsequenced");
}

/// A driver that takes a backwards CSeq as it comes draws no verdict for it.
#[test]
fn backwards_accepted_by_policy() {
    let mut t = ringing_uas(BackwardsCseq::Accept);
    assert_eq!(
        t.judge_received(&request("PRACK", 4, "A1", Some("B1")), 0).map(|v| v.rule()),
        Some(DIALOG_CSEQ_SKIP)
    );
    assert_eq!(t.judge_received(&request("UPDATE", 3, "A1", Some("B1")), 0), None);
}

/// A non-2xx final to the INVITE ends its early dialogs (§12.1.2): a request
/// on one is on an ended dialog; the ACK of the final is not.
#[test]
fn a_non_2xx_final_ends_the_early_dialogs() {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.note_sent_response(&response(487, "INVITE", 1, "A1", "B1"), 0);
    assert_eq!(t.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None);
    assert_eq!(
        t.judge_received(&request("PRACK", 2, "A1", Some("B1")), 0),
        None,
        "a PRACK owed before the final may follow it (RFC 3262 §3)"
    );
    let v = t.judge_received(&request("UPDATE", 3, "A1", Some("B1")), 0).expect("ended");
    assert_eq!(v.kind, VerdictKind::Ended { by: Some(487) });
    assert_eq!(v.to_string(), "UPDATE From-tag A1 To-tag B1 names a dialog a 487 final ended");
}

/// A BYE ends the dialog once its transaction took a final: a later request
/// is on an ended dialog, a retransmitted BYE is not judged again.
#[test]
fn a_bye_ends_the_dialog_after_its_final() {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.note_sent_response(&response(200, "INVITE", 1, "A1", "B1"), 0);
    assert_eq!(t.judge_received(&request("BYE", 2, "A1", Some("B1")), 0), None);
    t.note_sent_response(&response(200, "BYE", 2, "A1", "B1"), 0);
    assert_eq!(t.judge_received(&request("BYE", 2, "A1", Some("B1")), 0), None, "a repeat");
    let v = t.judge_received(&request("OPTIONS", 3, "A1", Some("B1")), 0).expect("ended");
    assert_eq!(v.kind, VerdictKind::Ended { by: None });
    assert_eq!(t.state(CALL, "B1", "A1"), Some(DialogState::EndedByBye));
}

/// The BYE a UA sent ends its dialog only at the BYE's final: a BYE crossing
/// it is taken, a request after the final is not.
#[test]
fn a_sent_bye_ends_the_dialog_at_its_final() {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.note_sent_response(&response(200, "INVITE", 1, "A1", "B1"), 0);
    t.note_sent_request(&{
        // The UAS's own BYE: From its tag B1, To the caller's A1.
        let text = format!(
            "BYE sip:a@host SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-b1\r\n\
             From: <sip:b@host>;tag=B1\r\nTo: <sip:a@host>;tag=A1\r\nCall-ID: {CALL}\r\n\
             CSeq: 1 BYE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        );
        match parse(text) {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => unreachable!(),
        }
    });
    assert_eq!(t.judge_received(&request("BYE", 2, "A1", Some("B1")), 0), None, "crossing");
    let ok = parse(format!(
        "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-b1\r\n\
         From: <sip:b@host>;tag=B1\r\nTo: <sip:a@host>;tag=A1\r\nCall-ID: {CALL}\r\n\
         CSeq: 1 BYE\r\nContent-Length: 0\r\n\r\n"
    ));
    let SipMessage::Response(ok) = ok else { unreachable!() };
    t.note_received_response(&ok, 0);
    assert_eq!(
        kind(t.judge_received(&request("INFO", 3, "A1", Some("B1")), 0)),
        Some(VerdictKind::Ended { by: None })
    );
}

/// A UAC whose callee rang fork `F1` and answered on `F3`, which never rang,
/// at 1 s on the table's clock.
fn answered_on_an_unrung_fork() -> DialogTable {
    let mut t = DialogTable::new(BackwardsCseq::Verdict);
    t.note_sent_request(&request("INVITE", 1, "A1", None));
    t.note_received_response(&response(180, "INVITE", 1, "A1", "F1"), 0);
    t.note_received_response(&response(200, "INVITE", 1, "A1", "F3"), 1_000_000);
    t
}

/// A request from the callee, From its fork tag, To the caller's tag.
fn from_fork(method: &str, cseq: u32, fork: &str) -> SipRequest {
    let text = format!(
        "{method} sip:a@host SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\nVia: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-{fork}{cseq}\r\n\
         From: <sip:b@host>;tag={fork}\r\nTo: <sip:a@host>;tag=A1\r\nCall-ID: {CALL}\r\n\
         CSeq: {cseq} {method}\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    );
    match parse(text) {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => unreachable!(),
    }
}

/// §13.2.2.4: once fork 3's 2xx confirmed the caller's dialog, fork 1's early
/// dialog stands 64·T1; past it, a request under its tag names no dialog.
#[test]
fn an_abandoned_early_dialog_stands_for_the_window() {
    use rfc_rules::rules::correlation::EARLY_DIALOG_WINDOW_US;
    let mut t = answered_on_an_unrung_fork();
    assert_eq!(t.judge_received(&from_fork("OPTIONS", 7, "F3"), 2_000_000), None, "UAC: empty");
    let inside = 1_000_000 + EARLY_DIALOG_WINDOW_US;
    assert_eq!(t.judge_received(&from_fork("UPDATE", 3, "F1"), inside), None);
    let v = t.judge_received(&from_fork("UPDATE", 4, "F1"), inside + 1).expect("past the window");
    assert_eq!(v.to_string(), "UPDATE From-tag F1 names no dialog (holds F3)");
    let v = t.judge_received(&from_fork("INFO", 1, "F9"), 2_000_000).expect("never seen");
    assert_eq!(v.rule(), DIALOG_UNKNOWN);
}

/// The answering side never BYEs an early dialog (§15) nor re-INVITEs one
/// (§14.1): under fork 1's tag they name no dialog, inside the window too.
#[test]
fn a_bye_or_invite_on_an_abandoned_early_dialog_names_no_dialog() {
    for method in ["BYE", "INVITE"] {
        let mut t = answered_on_an_unrung_fork();
        let v = t.judge_received(&from_fork(method, 2, "F1"), 1_000_001);
        assert_eq!(v.map(|v| v.rule()), Some(DIALOG_UNKNOWN), "{method}");
    }
}

/// The hop ACK of a non-2xx final the UA sent on no dialog (its 481) names
/// that refusal, not a dialog; any other ACK on no dialog is judged.
#[test]
fn the_ack_of_a_refusal_is_not_judged() {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.note_sent_response(&response(481, "INVITE", 9, "A7", "B7"), 0);
    assert_eq!(t.judge_received(&request("ACK", 9, "A7", Some("B7")), 0), None);
    assert_eq!(
        t.judge_received(&request("ACK", 9, "A8", Some("B8")), 0).map(|v| v.rule()),
        Some(DIALOG_UNKNOWN)
    );
}

/// A NOTIFY with its Subscription-State.
fn notify(cseq: u32, state: &str) -> SipRequest {
    let text = format!(
        "NOTIFY sip:b@host SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-n{cseq}\r\n\
         From: <sip:a@host>;tag=A1\r\nTo: <sip:b@host>;tag=B1\r\nCall-ID: {CALL}\r\n\
         CSeq: {cseq} NOTIFY\r\nEvent: refer\r\nSubscription-State: {state}\r\n\
         Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    );
    match parse(text) {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => unreachable!(),
    }
}

/// A REFER's implicit subscription outlives the BYE that ends the invite
/// usage (RFC 5057 §5.2): its NOTIFYs stay legal until one terminates it.
#[test]
fn a_referrers_notifies_outlive_its_bye() {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.note_sent_response(&response(200, "INVITE", 1, "A1", "B1"), 0);
    let refer = parse(format!(
        "REFER sip:a@host SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\nVia: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-r1\r\n\
         From: <sip:b@host>;tag=B1\r\nTo: <sip:a@host>;tag=A1\r\nCall-ID: {CALL}\r\n\
         CSeq: 1 REFER\r\nRefer-To: <sip:c@host>\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    ));
    let SipMessage::Request(refer) = refer else { unreachable!() };
    t.note_sent_request(&refer);
    assert_eq!(t.judge_received(&request("BYE", 2, "A1", Some("B1")), 0), None);
    t.note_sent_response(&response(200, "BYE", 2, "A1", "B1"), 0);
    assert_eq!(t.judge_received(&notify(3, "active;expires=60"), 0), None);
    assert_eq!(t.judge_received(&notify(4, "terminated;reason=noresource"), 0), None);
    assert_eq!(
        kind(t.judge_received(&notify(5, "active"), 0)),
        Some(VerdictKind::Ended { by: None }),
        "the subscription ended with its terminating NOTIFY"
    );
}

/// A UAC (local tag `A1`) whose INVITE (CSeq 1) rang on fork `B1`.
fn ringing_uac() -> DialogTable {
    let mut t = DialogTable::new(BackwardsCseq::Verdict);
    t.note_sent_request(&request("INVITE", 1, "A1", None));
    t.note_received_response(&response(180, "INVITE", 1, "A1", "B1"), 0);
    t
}

/// A request the UAC sent on its early dialog with `B1`.
fn uac_request(method: &str, cseq: u32) -> SipRequest {
    request(method, cseq, "A1", Some("B1"))
}

/// RFC 3261 §12.1.2: a non-2xx final ended the early dialog, so a 2xx to a
/// request the UA sent on it answers a dialog that is gone. The message names
/// the status, the method, the final and the dialog.
#[test]
fn a_2xx_on_a_dialog_a_final_ended_is_a_verdict() {
    let mut t = ringing_uac();
    t.note_sent_request(&uac_request("BYE", 2));
    assert_eq!(t.note_received_response(&response(487, "INVITE", 1, "A1", "B1"), 0), None);
    let v = t.note_received_response(&response(200, "BYE", 2, "A1", "B1"), 0).expect("a verdict");
    assert_eq!(v.rule(), DIALOG_ENDED_ANSWERED);
    assert_eq!(
        v.to_string(),
        format!("200 to BYE on a dialog a 487 ended (Call-ID {CALL}, tags A1/B1)")
    );
    assert_eq!(v.kind.refusal(), None, "a response is taken");
}

/// A UAS (local tag `B1`) that sent a 183 to the caller `A1`'s INVITE (CSeq
/// 1), an INFO (CSeq 1) on that early dialog, then a 487 to the INVITE.
fn refused_after_its_own_info() -> DialogTable {
    refused_through(0)
}

/// [`refused_after_its_own_info`], the INVITE arriving through `proxies` hops.
fn refused_through(proxies: usize) -> DialogTable {
    let mut t = DialogTable::new(BackwardsCseq::Verdict);
    assert_eq!(t.judge_received(&request_through("INVITE", 1, "A1", None, proxies), 0), None);
    t.note_sent_response(&response(183, "INVITE", 1, "A1", "B1"), 0);
    t.note_sent_request(&request("INFO", 1, "B1", Some("A1")));
    t.note_sent_response(&response(487, "INVITE", 1, "A1", "B1"), 0);
    t
}

/// The caller's 200 to the UAS's INFO, From the UAS's tag `B1`.
fn info_ok() -> SipResponse {
    response(200, "INFO", 1, "B1", "A1")
}

/// RFC 3261 §12.3, §17.2.1: until the peer's ACK of the final the UA sent
/// reaches it, the peer may answer a request it took on the live early
/// dialog; an ACK of another INVITE, or a retransmission of the INVITE,
/// proves nothing.
#[test]
fn a_2xx_crossing_the_uas_own_final_is_no_verdict() {
    let mut t = refused_after_its_own_info();
    assert_eq!(t.judge_received(&request("ACK", 2, "A1", Some("B1")), 0), None);
    assert_eq!(t.judge_received(&request("INVITE", 1, "A1", None), 0), None);
    assert_eq!(t.note_received_response(&info_ok(), 0), None);
    assert_eq!(t.state(CALL, "B1", "A1"), Some(DialogState::EndedByFinal(487)));
}

/// Once the peer's ACK of the UA's final arrived, the peer knew the dialog
/// ended: a 2xx it sends after is a verdict.
#[test]
fn a_2xx_after_the_ack_of_the_uas_own_final_is_a_verdict() {
    let mut t = refused_after_its_own_info();
    assert_eq!(t.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None);
    assert_eq!(
        kind(t.note_received_response(&info_ok(), 0)),
        Some(VerdictKind::EndedAnswered { status: 200, by: 487 })
    );
}

/// A new INVITE of the same caller follows the final of its first one (RFC
/// 3261 §8.1.3.5): a 2xx after it is a verdict, its ACK unseen.
#[test]
fn a_2xx_after_the_callers_retry_invite_is_a_verdict() {
    let mut t = refused_after_its_own_info();
    assert_eq!(t.judge_received(&request("INVITE", 2, "A1", None), 0), None);
    assert_eq!(
        kind(t.note_received_response(&info_ok(), 0)),
        Some(VerdictKind::EndedAnswered { status: 200, by: 487 })
    );
}

/// RFC 3261 §17.1.1.3: a transaction-stateful proxy ACKs the UA's non-2xx
/// final itself, under its own single Via, before the caller sees the final:
/// that ACK proves nothing, so a 2xx after it may still cross the final.
#[test]
fn a_hops_own_ack_of_the_uas_final_proves_nothing() {
    let mut t = refused_through(1);
    assert_eq!(t.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None);
    assert_eq!(t.note_received_response(&info_ok(), 0), None);
}

/// The caller's own ACK of the UA's final, relayed by a proxy, carries the
/// INVITE's Via stack as the UA received it: a 2xx after it is a verdict.
#[test]
fn the_callers_ack_through_a_proxy_shows_it_saw_the_uas_final() {
    let mut t = refused_through(1);
    assert_eq!(t.judge_received(&request_through("ACK", 1, "A1", Some("B1"), 1), 0), None);
    assert_eq!(
        kind(t.note_received_response(&info_ok(), 0)),
        Some(VerdictKind::EndedAnswered { status: 200, by: 487 })
    );
}

/// The caller's INFO (CSeq 2) on the early dialog the UAS's 487 ended.
fn callers_info() -> SipRequest {
    request("INFO", 2, "A1", Some("B1"))
}

/// RFC 3261 §12.3, §17.2.1: the caller may send a request on the early
/// dialog before the UAS's final reaches it. The UAS refuses it 481 and it is
/// no finding; its retransmission after the ACK is not judged again.
#[test]
fn a_request_crossing_the_uas_own_final_is_refused_and_no_finding() {
    let mut t = refused_after_its_own_info();
    let v = t.judge_received(&callers_info(), 0).expect("refused");
    assert_eq!(v.kind, VerdictKind::CrossedFinal { by: 487, broke: None });
    assert_eq!(v.rule(), DIALOG_ENDED);
    assert_eq!(v.kind.refusal(), Some((481, "Call/Transaction Does Not Exist")));
    assert!(!v.kind.is_finding());
    assert_eq!(t.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None);
    assert_eq!(t.judge_received(&callers_info(), 0), None, "a retransmission");
}

/// Once the caller's ACK of the UAS's final arrived, a request it sends on
/// the dialog is one it knew ended; a hop's own ACK shows nothing of that.
#[test]
fn a_request_after_the_ack_of_the_uas_own_final_is_a_finding() {
    let mut t = refused_after_its_own_info();
    assert_eq!(t.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None);
    let v = t.judge_received(&callers_info(), 0).expect("a verdict");
    assert_eq!(v.kind, VerdictKind::Ended { by: Some(487) });
    assert!(v.kind.is_finding());

    let mut through = refused_through(1);
    assert_eq!(through.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None);
    let v = through.judge_received(&callers_info(), 0).expect("refused");
    assert_eq!(v.kind, VerdictKind::CrossedFinal { by: 487, broke: None });
}

/// A non-2xx final with no provisional before it is the first message to
/// carry its To-tag: a request naming that tag follows the final, so it is a
/// finding.
#[test]
fn a_request_on_the_tag_of_a_final_with_no_provisional_is_a_finding() {
    let mut t = DialogTable::new(BackwardsCseq::Verdict);
    assert_eq!(t.judge_received(&request("INVITE", 1, "A1", None), 0), None);
    t.note_sent_response(&response(487, "INVITE", 1, "A1", "B1"), 0);
    assert_eq!(
        kind(t.judge_received(&callers_info(), 0)),
        Some(VerdictKind::Ended { by: Some(487) })
    );
}

/// RFC 3261 §17.2.2: the peer's server transaction replays its 2xx for each
/// retransmission of the request it answers. A 2xx the UA took on a dialog
/// is judged once: its replay after the ACK of the UA's final is no verdict.
#[test]
fn a_replayed_crossing_2xx_after_the_ack_is_no_verdict() {
    let mut t = refused_after_its_own_info();
    assert_eq!(t.note_received_response(&info_ok(), 0), None);
    assert_eq!(t.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None);
    assert_eq!(t.note_received_response(&info_ok(), 0), None);
}

/// The 481 such a request deserves, a PRACK's 2xx (RFC 3262 §3) and a 2xx
/// before the final draw nothing.
#[test]
fn a_481_a_prack_200_and_a_2xx_before_the_final_are_no_verdict() {
    let mut t = ringing_uac();
    assert_eq!(t.note_received_response(&response(200, "UPDATE", 2, "A1", "B1"), 0), None);
    t.note_received_response(&response(487, "INVITE", 1, "A1", "B1"), 0);
    assert_eq!(t.note_received_response(&response(481, "BYE", 3, "A1", "B1"), 0), None);
    assert_eq!(t.note_received_response(&response(200, "PRACK", 4, "A1", "B1"), 0), None);
}

/// §15: the callee never BYEs an early dialog; the caller may.
#[test]
fn a_callees_bye_on_an_early_dialog_is_a_verdict() {
    let mut t = ringing_uac();
    let bye = parse(format!(
        "BYE sip:a@host SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-eb\r\n\
         From: <sip:b@host>;tag=B1\r\nTo: <sip:a@host>;tag=A1\r\nCall-ID: {CALL}\r\n\
         CSeq: 1 BYE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    ));
    let SipMessage::Request(bye) = bye else { unreachable!() };
    let v = t.judge_received(&bye, 0).expect("a verdict");
    assert_eq!(v.rule(), DIALOG_EARLY_REQUEST);
    assert_eq!(v.kind.refusal(), None);
    assert!(v.to_string().starts_with("BYE on the early dialog From-tag B1 To-tag A1"), "{v}");

    let mut uas = ringing_uas(BackwardsCseq::Verdict);
    assert_eq!(uas.judge_received(&request("BYE", 2, "A1", Some("B1")), 0), None, "the caller may");
}

/// §14.1: no re-INVITE on an early dialog while its INVITE is in progress,
/// from either side; the UA answers 491.
#[test]
fn a_reinvite_on_an_early_dialog_is_a_verdict() {
    let mut uas = ringing_uas(BackwardsCseq::Verdict);
    let v = uas.judge_received(&request("INVITE", 2, "A1", Some("B1")), 0).expect("a verdict");
    assert_eq!(v.rule(), DIALOG_EARLY_REQUEST);
    assert_eq!(v.kind.refusal(), Some((491, "Request Pending")));
}

/// RFC 4488: a REFER stating `Refer-Sub: false` opens no subscription, so a
/// NOTIFY after the BYE is on an ended dialog.
#[test]
fn a_refer_sub_false_refer_opens_no_subscription() {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.note_sent_response(&response(200, "INVITE", 1, "A1", "B1"), 0);
    let refer = parse(format!(
        "REFER sip:a@host SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\nVia: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-r1\r\n\
         From: <sip:b@host>;tag=B1\r\nTo: <sip:a@host>;tag=A1\r\nCall-ID: {CALL}\r\n\
         CSeq: 1 REFER\r\nRefer-To: <sip:c@host>\r\nRefer-Sub: false\r\n\
         Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    ));
    let SipMessage::Request(refer) = refer else { unreachable!() };
    t.note_sent_request(&refer);
    assert_eq!(t.judge_received(&request("BYE", 2, "A1", Some("B1")), 0), None);
    t.note_sent_response(&response(200, "BYE", 2, "A1", "B1"), 0);
    assert_eq!(
        kind(t.judge_received(&notify(3, "active"), 0)),
        Some(VerdictKind::Ended { by: None })
    );
}

/// A message with the header line `extra` added after its Call-ID.
fn with_line(text: String, extra: &str) -> SipMessage {
    parse(text.replacen("Call-ID:", &format!("{extra}\r\nCall-ID:"), 1))
}

/// RFC 3262 §3: a PRACK whose `RAck` names a reliable provisional the UA sent
/// on its dialog is owed a 2xx; one naming another RSeq, or riding another
/// dialog, names nothing.
#[test]
fn a_prack_names_the_reliable_provisional_the_ua_sent() {
    let mut t = super::reliable::ReliableSent::default();
    let text = format!(
        "SIP/2.0 183 Session Progress\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1INVITE\r\n\
         From: {}\r\nTo: {}\r\nCall-ID: {CALL}\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        tagged("a", Some("A1")),
        tagged("b", Some("B1")),
    );
    let SipMessage::Response(reliable) = with_line(text, "Require: 100rel\r\nRSeq: 7") else {
        unreachable!()
    };
    t.note(&reliable);
    let prack = |rack: &str, to: &str| {
        let text = format!(
            "PRACK sip:x@host SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-2PRACK\r\n\
             From: {}\r\nTo: {}\r\nCall-ID: {CALL}\r\nCSeq: 2 PRACK\r\n\
             Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n",
            tagged("a", Some("A1")),
            tagged("b", Some(to)),
        );
        let SipMessage::Request(r) = with_line(text, &format!("RAck: {rack}")) else {
            unreachable!()
        };
        r
    };
    assert!(t.names(&prack("7 1 INVITE", "B1")));
    assert!(!t.names(&prack("8 1 INVITE", "B1")));
    assert!(!t.names(&prack("7 1 INVITE", "B9")));
}

/// A request of the dialog `from_tag` the loss model dropped on its way to
/// the UA.
fn lost_request(method: &str, from_tag: &str, cseq: u32) -> crate::realcall::LostMessage {
    crate::realcall::LostMessage {
        leg: None,
        inbound: true,
        method: Some(method.to_string()),
        status: None,
        cseq_method: method.to_string(),
        sdp: false,
        reliable: false,
        to_tag: String::new(),
        call_id: "c1".to_string(),
        from_tag: from_tag.to_string(),
        cseq,
        arrived_before: vec![],
    }
}

fn verdict_of(method: &str, cseq: u32, kind: VerdictKind) -> DialogVerdict {
    DialogVerdict {
        method: method.to_string(),
        call_id: "c1".to_string(),
        from_tag: "a".to_string(),
        to_tag: "b".to_string(),
        cseq,
        terminating: false,
        kind,
    }
}

/// A verdict the loss model's drops explain: a skip over the CSeq a dropped
/// request of the dialog carried; a backwards CSeq, or a request on a dialog
/// a BYE ended, that retransmits a dropped copy. A drop on another dialog,
/// or of another request, explains nothing.
#[test]
fn a_verdict_is_explained_only_by_a_drop_of_its_own_request() {
    let skip = verdict_of("BYE", 4, VerdictKind::CseqSkip { cseq: 4, last: 2 });
    let prack3 = lost_request("PRACK", "a", 3);
    assert!(skip.explained_by(&[&prack3]));
    assert!(!skip.explained_by(&[&lost_request("PRACK", "z", 3)]), "another dialog");
    assert!(!skip.explained_by(&[&lost_request("PRACK", "a", 4)]), "not in the gap");
    let ended = verdict_of("PRACK", 3, VerdictKind::Ended { by: None });
    assert!(ended.explained_by(&[&prack3]));
    assert!(!ended.explained_by(&[&lost_request("INFO", "a", 3)]), "another request");
    let backwards = verdict_of("OPTIONS", 4, VerdictKind::CseqBackwards { cseq: 4, last: 5 });
    assert!(backwards.explained_by(&[&lost_request("OPTIONS", "a", 4)]));
    let by_final = verdict_of("PRACK", 3, VerdictKind::Ended { by: Some(487) });
    assert!(!by_final.explained_by(&[&prack3]), "a final's end is not the loss's");
}

/// A request crossing the UA's final is explained as the rule it broke on
/// the early dialog is.
#[test]
fn a_crossing_verdict_is_explained_as_the_rule_it_broke() {
    let broke = Some(Box::new(VerdictKind::CseqSkip { cseq: 4, last: 2 }));
    let skip = verdict_of("INFO", 4, VerdictKind::CrossedFinal { by: 487, broke });
    assert!(skip.explained_by(&[&lost_request("INFO", "a", 3)]));
    assert!(!skip.explained_by(&[&lost_request("INFO", "a", 4)]), "not in the gap");
}

const REFUSED_481: Option<(u16, &str)> = Some((481, "Call/Transaction Does Not Exist"));

/// The verdict on `request`, received by `t`, as a crossing request that
/// breaks `rule`: a finding of that rule, still refused 481 on the ended
/// dialog.
fn assert_crossing_breaks(t: &mut DialogTable, request: &SipRequest, rule: &str) {
    let v = t.judge_received(request, 0).expect("a verdict");
    assert_eq!((v.rule(), v.kind.is_finding()), (rule, true), "got {:?}", v.kind);
    assert_eq!(v.kind.refusal(), REFUSED_481, "got {:?}", v.kind);
}

/// RFC 3261 §14.1: a UAC does not start a re-INVITE while its INVITE is in
/// progress, so one on the early dialog, crossing the UAS's final, is still a
/// finding.
#[test]
fn a_reinvite_crossing_the_uas_own_final_is_a_finding() {
    let mut t = refused_after_its_own_info();
    assert_crossing_breaks(&mut t, &request("INVITE", 2, "A1", Some("B1")), DIALOG_EARLY_REQUEST);
}

/// RFC 3261 §12.2.1.1: the caller numbers its requests on the early dialog
/// one up from its INVITE's; a crossing request that skips is a finding.
#[test]
fn a_cseq_skip_crossing_the_uas_own_final_is_a_finding() {
    let mut t = refused_after_its_own_info();
    assert_crossing_breaks(&mut t, &request("INFO", 5, "A1", Some("B1")), DIALOG_CSEQ_SKIP);
}

/// RFC 3261 §12.2.2: a crossing request numbered below the INVITE's is a
/// finding.
#[test]
fn a_cseq_backwards_crossing_the_uas_own_final_is_a_finding() {
    let mut t = DialogTable::new(BackwardsCseq::Verdict);
    assert_eq!(t.judge_received(&request("INVITE", 3, "A1", None), 0), None);
    t.note_sent_response(&response(183, "INVITE", 3, "A1", "B1"), 0);
    t.note_sent_response(&response(487, "INVITE", 3, "A1", "B1"), 0);
    assert_crossing_breaks(&mut t, &request("INFO", 2, "A1", Some("B1")), DIALOG_CSEQ_BACKWARDS);
}

/// A crossing request advances the remote sequence: the next one in order
/// is no finding, one past it skips.
#[test]
fn a_crossing_request_advances_the_remote_cseq() {
    let mut t = refused_after_its_own_info();
    for cseq in [2, 3] {
        let v = t.judge_received(&request("INFO", cseq, "A1", Some("B1")), 0).expect("refused");
        assert!(!v.kind.is_finding(), "CSeq {cseq}: got {:?}", v.kind);
        assert_eq!(v.kind.refusal(), REFUSED_481);
    }
    assert_crossing_breaks(&mut t, &request("INFO", 5, "A1", Some("B1")), DIALOG_CSEQ_SKIP);
}

/// A crossing request whose CSeq goes backwards, under
/// [`BackwardsCseq::Accept`], is no finding and still refused: the dialog
/// ended, so nothing on it is taken.
#[test]
fn a_backwards_crossing_request_accepted_is_still_refused() {
    let mut t = DialogTable::new(BackwardsCseq::Accept);
    assert_eq!(t.judge_received(&request("INVITE", 3, "A1", None), 0), None);
    t.note_sent_response(&response(183, "INVITE", 3, "A1", "B1"), 0);
    t.note_sent_response(&response(487, "INVITE", 3, "A1", "B1"), 0);
    let v = t.judge_received(&request("INFO", 2, "A1", Some("B1")), 0).expect("refused");
    assert!(!v.kind.is_finding(), "got {:?}", v.kind);
    assert_eq!(v.kind.refusal(), REFUSED_481);
}
