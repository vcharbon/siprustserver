//! The driver is strict: a step the call gives nothing to say panics, unless
//! declared optional, and a step that takes no turn panics where a turn is
//! expected.

use super::{Driver, Outcome, Step, ALICE, ALICE_TAG, CALL_ID};

#[test]
#[should_panic(expected = "finds nothing to do")]
fn a_step_with_nothing_to_do_panics() {
    // alice holds no dialog before any provisional: her ACK names none.
    Driver::new().dial().step(Step::AliceAcks);
}

#[test]
fn an_optional_step_with_nothing_to_do_is_an_outcome() {
    let driver = Driver::new();
    let mut call = driver.dial();
    assert!(matches!(call.try_step(Step::AliceAcks), Outcome::Nothing));
}

/// alice's INFO naming a dialog the call never had.
fn stray_info() -> Step {
    let info = format!(
        "INFO sip:svc@10.0.0.1:5060 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {ALICE};branch=z9hG4bK-stray\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag={ALICE_TAG}\r\n\
         To: <sip:bob@example.com>;tag=nosuchtag\r\n\
         Call-ID: {CALL_ID}\r\n\
         CSeq: 5 INFO\r\n\
         Content-Length: 0\r\n\r\n"
    );
    Step::Raw("a", info)
}

#[test]
fn the_gates_answer_is_an_outcome() {
    let driver = Driver::new();
    let mut call = driver.dial();
    assert_eq!(call.step(stray_info()).gate_status(), Some(481));
}

#[test]
#[should_panic(expected = "takes no turn")]
fn a_step_the_gate_answers_takes_no_turn() {
    let driver = Driver::new();
    driver.dial().turned(stray_info());
}
