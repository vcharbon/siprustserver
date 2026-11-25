use std::sync::{Arc, Mutex};

use tokio::time::Instant;

use super::*;
use crate::mux::datagram::Datagram;
use crate::mux::registry::SocketRegistry;
use crate::mux::stats::MuxStats;
use crate::mux::tally::{ArrowKey, CallTally};
use crate::mux::{Key, RELEASE_HOLD};

fn req(method: &str, cseq: u32, branch: &str) -> Vec<u8> {
    format!(
        "{method} sip:bob@192.0.2.2 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\nVia: SIP/2.0/UDP 192.0.2.1;branch={branch}\r\n\
         From: <sip:alice@192.0.2.1>;tag=a1\r\nTo: <sip:bob@192.0.2.2>;tag=b1\r\n\
         Call-ID: c1\r\nCSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn resp(status: u16, method: &str, cseq: u32, branch: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} Reason\r\nContact: <sip:fixture@192.0.2.98>\r\nVia: SIP/2.0/UDP 192.0.2.1;branch={branch}\r\n\
         From: <sip:alice@192.0.2.1>;tag=a1\r\nTo: <sip:bob@192.0.2.2>;tag=b1\r\n\
         Call-ID: c1\r\nCSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn held(sent: &[Vec<u8>]) -> HeldAnswers {
    let answers = SentAnswers::default();
    for raw in sent {
        answers.on_outbound(&Datagram::sniff(raw), None);
    }
    answers.take("c1").expect("the dialog holds answers")
}

fn reanswer(h: &mut HeldAnswers, raw: &[u8]) -> Option<Vec<u8>> {
    match on_request(h, raw) {
        LateReply::Reanswer(bytes, _) => Some(bytes.to_vec()),
        _ => None,
    }
}

fn on_request(h: &mut HeldAnswers, raw: &[u8]) -> LateReply {
    h.on_request(&Datagram::sniff(raw))
}

#[test]
#[ignore = "slow lane: loadgen"]
fn a_retransmitted_bye_draws_its_final_and_another_request_stays_stray() {
    let ok = resp(200, "BYE", 3, "z9hG4bKbye");
    let mut h = held(std::slice::from_ref(&ok));
    assert_eq!(reanswer(&mut h, &req("BYE", 3, "z9hG4bKbye")), Some(ok), "the same final");
    for (raw, why) in [
        (req("BYE", 3, "z9hG4bKother"), "another branch: another transaction"),
        (req("BYE", 4, "z9hG4bKbye"), "another CSeq"),
        (req("INFO", 3, "z9hG4bKbye"), "another method"),
        (req("ACK", 3, "z9hG4bKbye"), "no INVITE final held to ACK"),
    ] {
        assert!(matches!(on_request(&mut h, &raw), LateReply::NotHeld), "{why}");
    }
}

#[test]
#[ignore = "slow lane: loadgen"]
fn a_cancelled_invite_holds_the_cancel_final_and_the_487_and_absorbs_its_ack() {
    let branch = "z9hG4bKinv";
    let (cancel_ok, terminated) = (resp(200, "CANCEL", 1, branch), resp(487, "INVITE", 1, branch));
    let mut h = held(&[resp(180, "INVITE", 1, branch), cancel_ok.clone(), terminated.clone()]);
    assert_eq!(reanswer(&mut h, &req("CANCEL", 1, branch)), Some(cancel_ok), "CANCEL's own final");
    assert_eq!(reanswer(&mut h, &req("INVITE", 1, branch)), Some(terminated), "the INVITE's 487");
    assert!(matches!(on_request(&mut h, &req("ACK", 1, branch)), LateReply::Absorbed), "its ACK");
    assert!(matches!(on_request(&mut h, &req("ACK", 1, "z9hG4bKx")), LateReply::NotHeld));
    assert!(
        matches!(on_request(&mut h, &req("INVITE", 1, branch)), LateReply::Absorbed),
        "once ACKed, a retransmitted INVITE draws its 487 no more"
    );
    assert!(matches!(on_request(&mut h, &req("ACK", 1, branch)), LateReply::Absorbed));
}

#[test]
#[ignore = "slow lane: loadgen"]
fn an_invite_final_acked_during_the_call_is_not_answered_again() {
    let branch = "z9hG4bKinv";
    let answers = SentAnswers::default();
    answers.on_outbound(&Datagram::sniff(&resp(486, "INVITE", 1, branch)), None);
    answers.on_ack(Datagram::sniff(&req("ACK", 1, branch)).id());
    let mut h = answers.take("c1").expect("the hold stays for the ACK's retransmissions");
    assert!(matches!(on_request(&mut h, &req("INVITE", 1, branch)), LateReply::Absorbed));
    assert!(matches!(on_request(&mut h, &req("ACK", 1, branch)), LateReply::Absorbed));
}

#[test]
#[ignore = "slow lane: loadgen"]
fn provisionals_and_requests_are_not_held() {
    let answers = SentAnswers::default();
    answers.on_outbound(&Datagram::sniff(&resp(100, "BYE", 2, "z9hG4bKb")), None);
    answers.on_outbound(&Datagram::sniff(&resp(180, "INVITE", 1, "z9hG4bKi")), None);
    answers.on_outbound(&Datagram::sniff(&req("BYE", 2, "z9hG4bKb")), None);
    assert!(answers.take("c1").is_none(), "nothing a retransmission may draw from the hold");
}

/// The ACK of a re-INVITE's 2xx the agent sent, arriving once the call
/// released (its first 2xx lost, the 2xx retransmitted), is absorbed: it is
/// a transaction of its own under a new branch (RFC 3261 §13.2.2.4). The 2xx
/// itself is not held: a retransmitted INVITE draws nothing.
#[test]
#[ignore = "slow lane: loadgen"]
fn the_late_ack_of_an_invite_2xx_is_absorbed() {
    let answers = SentAnswers::default();
    let ok = resp(200, "INVITE", 2, "z9hG4bKreinv");
    answers.on_outbound(&Datagram::sniff(&ok), None);
    answers.on_outbound(&Datagram::sniff(&ok), None);
    answers.on_outbound(&Datagram::sniff(&resp(200, "BYE", 3, "z9hG4bKbye")), None);
    let mut h = answers.take("c1").expect("the 2xx leaves its marker");
    let ack = req("ACK", 2, "z9hG4bKack2");
    assert!(matches!(on_request(&mut h, &ack), LateReply::Absorbed), "the 2xx's ACK");
    assert!(matches!(on_request(&mut h, &ack), LateReply::Absorbed), "and its retransmission");
    assert!(
        matches!(on_request(&mut h, &req("ACK", 1, "z9hG4bKack1")), LateReply::NotHeld),
        "no 2xx sent for CSeq 1"
    );
    assert!(
        matches!(on_request(&mut h, &req("INVITE", 2, "z9hG4bKreinv")), LateReply::NotHeld),
        "the 2xx is the dialog's own, not re-sent by the hold"
    );
}

#[test]
#[ignore = "slow lane: loadgen"]
fn a_dialog_holds_its_last_answers_one_per_transaction() {
    let answers = SentAnswers::default();
    for cseq in 1..=ANSWERS_HELD as u32 + 2 {
        answers.on_outbound(
            &Datagram::sniff(&resp(200, "INFO", cseq, &format!("z9hG4bK{cseq}"))),
            None,
        );
    }
    let again = resp(481, "INFO", 10, "z9hG4bK10");
    answers.on_outbound(&Datagram::sniff(&again), None);
    let mut h = answers.take("c1").unwrap();
    assert_eq!(h.0.len(), ANSWERS_HELD, "bounded");
    assert!(reanswer(&mut h, &req("INFO", 2, "z9hG4bK2")).is_none(), "the oldest went");
    assert!(reanswer(&mut h, &req("INFO", 3, "z9hG4bK3")).is_some());
    assert_eq!(reanswer(&mut h, &req("INFO", 10, "z9hG4bK10")), Some(again), "the latest answer");
}

/// A callee that sent fifteen reliable provisionals answered fifteen PRACKs,
/// each its own transaction (RFC 3262 §3), then its INVITE's 2xx and the
/// BYE's 200: its first PRACK's 200 is still held when the call releases,
/// and a PRACK retransmitted until Timer F draws it, never a stray.
#[test]
#[ignore = "slow lane: loadgen"]
fn the_pracks_of_a_long_reliable_train_stay_answerable() {
    let mut sent: Vec<Vec<u8>> =
        (2..=16).map(|cseq| resp(200, "PRACK", cseq, &format!("z9hG4bKp{cseq}"))).collect();
    sent.push(resp(200, "INVITE", 1, "z9hG4bKinv"));
    sent.push(resp(200, "BYE", 17, "z9hG4bKbye"));
    let mut h = held(&sent);
    let first = reanswer(&mut h, &req("PRACK", 2, "z9hG4bKp2"));
    assert_eq!(first, Some(sent[0].clone()), "the first PRACK's 200");
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_held_answer_expires_with_the_hold() {
    let stats = MuxStats::new(0);
    let mut reg = SocketRegistry::default();
    let ok = resp(200, "BYE", 3, "z9hG4bKbye");
    let mut answers = Some(held(std::slice::from_ref(&ok)));
    let keys = vec![Key::CallId("c1".into())];
    reg.release_keys(keys, |_| (None, answers.take()), Instant::now() + RELEASE_HOLD, &stats);
    let bye = req("BYE", 3, "z9hG4bKbye");
    let bye = Datagram::sniff(&bye);
    let drawn = |reg: &mut SocketRegistry| match reg.released_answer("c1", &bye, Instant::now()) {
        LateReply::Reanswer(bytes, _) => Some(bytes.to_vec()),
        _ => None,
    };

    tokio::time::advance(RELEASE_HOLD - std::time::Duration::from_secs(1)).await;
    assert_eq!(drawn(&mut reg), Some(ok), "held for the hold");
    assert!(matches!(reg.released_answer("c2", &bye, Instant::now()), LateReply::NotHeld));
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert_eq!(drawn(&mut reg), None, "gone at the deadline");
    reg.expire_released(Instant::now());
    let keys = vec![Key::CallId("c1".into())];
    reg.release_keys(keys, |_| (None, None), Instant::now() + RELEASE_HOLD, &stats);
    assert_eq!(drawn(&mut reg), None, "freed by the sweep: a later release of c1 holds nothing");
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_late_reanswer_is_reported_on_the_arrow_of_its_request() {
    let seen: Arc<Mutex<Vec<ArrowKey>>> = Arc::default();
    let sink = seen.clone();
    let tally = CallTally::new("alice")
        .with_late_sink(Arc::new(move |arrow: &ArrowKey| sink.lock().unwrap().push(arrow.clone())));
    let taps = Taps::new(None, Some(Arc::new(tally)), None, None, None, Some("bob")).unwrap();
    let (bye, ok) = (req("BYE", 3, "z9hG4bKbye"), resp(200, "BYE", 3, "z9hG4bKbye"));
    taps.on_inbound(&Datagram::sniff(&bye), Instant::now());
    let answers = SentAnswers::default();
    answers.on_outbound(&Datagram::sniff(&ok), Some(&taps));
    let LateReply::Reanswer(_, Some(repair)) = on_request(&mut answers.take("c1").unwrap(), &bye)
    else {
        panic!("the re-answer carries its report");
    };
    repair.record();
    let want = ArrowKey { from: "SUT".into(), to: "bob".into(), label: "BYE".into(), nth: 1 };
    assert_eq!(*seen.lock().unwrap(), vec![want]);
}

/// A request that reached the endpoint, delivered or lost on its way in,
/// and that the agent never answered, is refused once the call released;
/// one the agent answered draws that answer, and an ACK draws nothing.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_request_in_unanswered_is_refused_unless_the_agent_answered_it() {
    let answers = SentAnswers::default();
    let bye = req("BYE", 3, "z9hG4bKbye");
    answers.on_request_in(&Datagram::sniff(&bye));
    answers.on_request_in(&Datagram::sniff(&req("ACK", 2, "z9hG4bKack")));
    let info = req("INFO", 4, "z9hG4bKinfo");
    answers.on_request_in(&Datagram::sniff(&info));
    let ok = resp(200, "INFO", 4, "z9hG4bKinfo");
    answers.on_outbound(&Datagram::sniff(&ok), None);
    answers.on_request_in(&Datagram::sniff(&info));
    let mut h = answers.take("c1").expect("the dialog holds answers");
    assert!(matches!(on_request(&mut h, &bye), LateReply::Refuse), "the unanswered BYE");
    assert_eq!(reanswer(&mut h, &info), Some(ok), "the agent's own answer stands");
    assert!(matches!(on_request(&mut h, &req("ACK", 2, "z9hG4bKack")), LateReply::NotHeld));
}

/// A CANCEL of an INVITE whose non-2xx final is held draws a 200 bearing that
/// final's To tag (RFC 3261 §9.2), its retransmission the same 200; a CANCEL
/// of an INVITE answered 2xx, or of a transaction not held, draws nothing
/// from the hold.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_cancel_of_an_answered_invite_draws_a_200_with_the_finals_tag() {
    let branch = "z9hG4bKinv";
    let busy = resp(486, "INVITE", 1, branch);
    let mut h = held(&[busy, resp(200, "INVITE", 2, "z9hG4bKok")]);
    for _ in 0..2 {
        let LateReply::Cancel(answers) = on_request(&mut h, &req("CANCEL", 1, branch)) else {
            panic!("the CANCEL is answered");
        };
        assert_eq!(answers.len(), 1, "a 200 alone: the INVITE has its final");
        let ok = Datagram::sniff(&answers[0]);
        let id = ok.id();
        assert_eq!((id.status, id.cseq, id.cseq_method), (Some(200), Some(1), Some("CANCEL")));
        assert_eq!((id.branch, id.to_tag), (Some(branch), Some("b1")));
    }
    for (raw, why) in [
        (req("CANCEL", 2, "z9hG4bKok"), "the 2xx ended the INVITE transaction"),
        (req("CANCEL", 1, "z9hG4bKother"), "another branch: another transaction"),
        (req("CANCEL", 3, branch), "another CSeq"),
    ] {
        assert!(matches!(on_request(&mut h, &raw), LateReply::NotHeld), "{why}");
    }
}

/// A re-INVITE that reached the endpoint and that the agent never answered
/// is cancelled once the call released: the CANCEL draws a 200 and the
/// re-INVITE a 487 (RFC 3261 §9.2), which the hold keeps as its final: a
/// retransmitted re-INVITE draws it again and its ACK is absorbed.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_cancel_of_an_unanswered_reinvite_draws_a_200_and_a_487() {
    let branch = "z9hG4bKreinv";
    let answers = SentAnswers::default();
    answers.on_request_in(&Datagram::sniff(&req("INVITE", 2, branch)));
    let mut h = answers.take("c1").expect("the unanswered re-INVITE is held");
    let LateReply::Cancel(sent) = on_request(&mut h, &req("CANCEL", 2, branch)) else {
        panic!("the CANCEL is answered");
    };
    let ids: Vec<_> = sent
        .iter()
        .map(|a| {
            let id = *Datagram::sniff(a).id();
            (id.status, id.cseq, id.cseq_method.map(str::to_owned), id.to_tag.map(str::to_owned))
        })
        .collect();
    let tagged =
        |status, method: &str| (Some(status), Some(2), Some(method.into()), Some("b1".into()));
    assert_eq!(ids, vec![tagged(200, "CANCEL"), tagged(487, "INVITE")]);
    assert_eq!(reanswer(&mut h, &req("INVITE", 2, branch)), Some(sent[1].to_vec()), "the 487 held");
    assert!(matches!(on_request(&mut h, &req("ACK", 2, branch)), LateReply::Absorbed), "its ACK");
    assert!(
        matches!(on_request(&mut h, &req("CANCEL", 2, branch)), LateReply::NotHeld),
        "once ACKed, a retransmitted CANCEL finds no transaction"
    );
}
