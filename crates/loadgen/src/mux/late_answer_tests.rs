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
    h.on_request(Datagram::sniff(raw).id())
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
fn an_invite_2xx_and_provisionals_are_not_held() {
    let answers = SentAnswers::default();
    answers.on_outbound(&Datagram::sniff(&resp(100, "BYE", 2, "z9hG4bKb")), None);
    answers.on_outbound(&Datagram::sniff(&resp(180, "INVITE", 1, "z9hG4bKi")), None);
    answers.on_outbound(&Datagram::sniff(&resp(200, "INVITE", 1, "z9hG4bKi")), None);
    answers.on_outbound(&Datagram::sniff(&req("BYE", 2, "z9hG4bKb")), None);
    assert!(answers.take("c1").is_none(), "nothing a retransmission may draw from the hold");
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
    let drawn = |reg: &mut SocketRegistry| match reg.released_answer("c1", bye.id(), Instant::now())
    {
        LateReply::Reanswer(bytes, _) => Some(bytes.to_vec()),
        _ => None,
    };

    tokio::time::advance(RELEASE_HOLD - std::time::Duration::from_secs(1)).await;
    assert_eq!(drawn(&mut reg), Some(ok), "held for the hold");
    assert!(matches!(reg.released_answer("c2", bye.id(), Instant::now()), LateReply::NotHeld));
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
    let taps = Taps::new(None, Some(Arc::new(tally)), None, Some("bob")).unwrap();
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
