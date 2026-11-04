use crate::cursor::tests::{
    answer, optional, optional_answer, plan, relayed, request_expect, send, status_only, D,
};
use crate::cursor::Cursor;

/// Two answers this leg is owed on two transactions it opened carry no
/// order between them (RFC 3261 §17): the BYE's 200 arms beside the
/// INVITE's 487 the document lists first, and whichever lands first is
/// taken first.
#[test]
fn an_answer_to_a_transaction_this_leg_opened_arms_beside_another_transaction_s_answer() {
    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "BYE"),
        answer("s3", "A", 487, "INVITE"),
        answer("s4", "A", 200, "BYE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3", "s4"], "both transactions are open: both armed");
    cursor.complete("s4");
    assert_eq!(cursor.frontier(), ["s3"], "the 487 is still owed");
    cursor.complete("s3");
    assert!(cursor.is_done());
}

/// The answer may be on the wire from the moment its request went out, so
/// a send this leg has not made yet cannot be what orders it — and walking
/// past that send never emits it early.
#[test]
fn an_answer_to_an_open_transaction_arms_behind_a_send() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "BYE"),
        answer("s3", "A", 487, "INVITE"),
        send("s4", "A", "ACK"),
        answer("s5", "A", 200, "BYE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3", "s5"], "the ACK behind the 487 is walked past");
    cursor.complete("s5");
    cursor.complete("s3");
    assert_eq!(cursor.frontier(), ["s4"], "the ACK arms only once its turn comes");
}

/// The request is the gate: an answer to a request this leg has not sent
/// yet is an arrival nothing has provoked, and list order is all there is.
#[test]
fn an_answer_whose_request_has_not_gone_out_waits_its_turn() {
    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "INVITE"),
        answer("s2", "A", 180, "INVITE"),
        send("s3", "A", "BYE"),
        answer("s4", "A", 200, "BYE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2"], "the BYE has not gone out");
}

/// Within ONE transaction list order binds: a provisional before its final.
/// Beside an answer on ANOTHER transaction, only the transaction's first
/// pending answer arms, and its final stays behind it.
#[test]
fn answers_on_one_transaction_keep_list_order() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "INVITE"),
        answer("s2", "A", 180, "INVITE"),
        answer("s3", "A", 200, "INVITE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2"], "the final waits behind the provisional");

    let q = plan(&format!(
        "[{},{},{},{},{}]",
        send("t1", "A", "INVITE"),
        send("t2", "A", "PRACK"),
        answer("t3", "A", 200, "PRACK"),
        answer("t4", "A", 180, "INVITE"),
        answer("t5", "A", 200, "INVITE")
    ));
    let mut other = Cursor::new(&q);
    other.complete("t1");
    other.complete("t2");
    assert_eq!(
        other.frontier(),
        ["t3", "t4"],
        "the INVITE's next provisional arms beside the PRACK's answer; its final does not"
    );
}

/// An expect gated on a status alone names no transaction, so nothing says
/// which request it answers, and it waits its turn.
#[test]
fn an_answer_naming_no_transaction_waits_its_turn() {
    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "BYE"),
        answer("s3", "A", 487, "INVITE"),
        status_only("s4", "A", 200)
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3"]);
}

/// A final already taken closed its transaction (RFC 3261 §17.1): a later
/// answer of the same method — a fork's second 2xx — is not an open
/// transaction's and keeps its place in the list.
#[test]
fn a_transaction_a_final_already_answered_opens_nothing() {
    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "INVITE"),
        answer("s2", "A", 200, "INVITE"),
        request_expect("s3", "A", "BYE"),
        answer("s4", "A", 200, "INVITE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3"], "the second 2xx waits behind the BYE");
}

/// The send that opened the transaction an expect waits on: the leg's last
/// send of that method before it, whatever stands between; none for a
/// status-only expect or a leg that never sent the method.
#[test]
fn the_opening_send_is_the_last_send_of_the_method_before_the_expect() {
    let p = plan(&format!(
        "[{},{},{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "PRACK"),
        answer("s3", "A", 200, "PRACK"),
        send("s4", "A", "PRACK"),
        answer("s5", "A", 481, "PRACK"),
        answer("s6", "A", 600, "INVITE"),
        status_only("s7", "A", 200)
    ));
    let cursor = Cursor::new(&p);
    assert_eq!(cursor.opening_send("s3"), Some("s2"));
    assert_eq!(cursor.opening_send("s5"), Some("s4"));
    assert_eq!(cursor.opening_send("s6"), Some("s1"));
    assert_eq!(cursor.opening_send("s7"), None, "a status alone names no transaction");
    assert_eq!(cursor.opening_send("s1"), None, "a send waits on nothing");
    let relay = plan(&format!("[{},{}]", send("s1", "B", "INVITE"), relayed("s2", "A", 183, "s1")));
    assert_eq!(Cursor::new(&relay).opening_send("s2"), None, "this leg sent no INVITE");
}

/// An armed `optional` answer with the candidate's own status refuses it,
/// even where it waits on an older transaction: two steps with one
/// discriminator armed together would take each other's datagram.
#[test]
fn an_answer_does_not_arm_beside_an_armed_answer_of_the_same_method() {
    let p = plan(&format!(
        "[{},{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "PRACK"),
        optional_answer("s3", "A", 200, "PRACK"),
        send("s4", "A", "PRACK"),
        request_expect("s5", "A", "INFO"),
        answer("s6", "A", 200, "PRACK")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    cursor.complete("s4");
    assert_eq!(cursor.frontier(), ["s3", "s5"], "s6 waits: s3 would take its 200");
}

/// An armed REQUIRED answer of the same method refuses the candidate
/// whatever its status: it is that method's earlier turn on the leg.
#[test]
fn an_answer_does_not_arm_beside_a_required_answer_of_the_same_method() {
    let p = plan(&format!(
        "[{},{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "PRACK"),
        answer("s3", "A", 200, "PRACK"),
        send("s4", "A", "PRACK"),
        request_expect("s5", "A", "INFO"),
        answer("s6", "A", 481, "PRACK")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    cursor.complete("s4");
    assert_eq!(cursor.frontier(), ["s3"], "s6 waits behind the required 200");
}

/// RFC 3261 §9.2: the CANCEL's 200 and the INVITE's 487 come in either
/// order. A tolerated absence pending on the INVITE in front of the 487 is
/// no earlier turn of that transaction, so the 487 arms beside the 200, and
/// its arrival releases the provisional that never came.
#[test]
fn a_tolerated_absence_in_front_does_not_hold_back_its_transaction_s_answer() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        optional("s2", "A", 183),
        send("s3", "A", "CANCEL"),
        answer("s4", "A", 200, "CANCEL"),
        answer("s5", "A", 487, "INVITE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s3");
    assert_eq!(cursor.frontier(), ["s2", "s4", "s5"], "the 487 arms beside the CANCEL's 200");
    assert_eq!(cursor.complete("s5"), ["s2"], "the 487 releases the 183 that never came");
    assert_eq!(cursor.frontier(), ["s4"]);
    cursor.complete("s4");
    assert!(cursor.is_done());
}

/// A tolerated absence of the SAME status still refuses the candidate: the
/// two steps would take each other's datagram.
#[test]
fn a_tolerated_absence_sharing_the_candidate_s_discriminator_still_holds_it_back() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "CANCEL"),
        optional("s3", "A", 487),
        answer("s4", "A", 200, "CANCEL"),
        answer("s5", "A", 487, "INVITE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3", "s4"], "s5 waits: s3 would take its 487");
}

fn answer_send(id: &str, leg: &str, status: u16, method: &str) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"send","msg":{{"status":{status},"cseq-method":"{method}"}},"delay":{D}}}"#
    )
}

/// RFC 3261 §9.2: the callee owes the 200 to a CANCEL and the INVITE's 487
/// at once. A BYE the capture shows arriving first does not gate them, so a
/// system that sends no such BYE is still answered; the BYE's own 200 waits.
#[test]
fn the_answers_a_received_cancel_draws_go_out_past_an_unrelated_arrival() {
    let p = plan(&format!(
        "[{},{},{},{},{},{},{}]",
        request_expect("s1", "B", "INVITE"),
        answer_send("s2", "B", 180, "INVITE"),
        request_expect("s3", "B", "CANCEL"),
        request_expect("s4", "B", "BYE"),
        answer_send("s5", "B", 200, "CANCEL"),
        answer_send("s6", "B", 200, "BYE"),
        answer_send("s7", "B", 487, "INVITE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3"], "nothing is owed before the CANCEL");
    cursor.complete("s3");
    assert_eq!(cursor.frontier(), ["s4", "s5", "s7"], "the BYE's 200 waits for the BYE");
}

/// A block standing between the leg's blocking step and the candidate stops
/// the walk: an `unordered` group is never walked past.
#[test]
fn an_unordered_group_in_front_stops_the_walk_to_an_open_transaction_s_answer() {
    let group = format!(
        r#"{{"id":"u1","op":"unordered","steps":[{},{}]}}"#,
        request_expect("s4", "A", "INFO"),
        request_expect("s5", "A", "UPDATE")
    );
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "BYE"),
        answer("s3", "A", 180, "INVITE"),
        group,
        answer("s6", "A", 200, "BYE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3"], "the group is not walked past");
}

/// A block that IS the leg's blocking item lets an answer to another open
/// transaction arm beside it: the branch heads are armed already, and a
/// candidate on their own transaction is refused.
#[test]
fn an_answer_arms_beside_an_armed_alt() {
    let alt = format!(
        r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"answered","steps":[{}]}},
                 {{"name":"busy","steps":[{}]}}]}}"#,
        answer("s3", "A", 200, "INVITE"),
        answer("s4", "A", 486, "INVITE")
    );
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "BYE"),
        alt,
        answer("s5", "A", 200, "BYE"),
        answer("s6", "A", 487, "INVITE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3", "s4", "s5"], "the INVITE's 487 is refused");
}

/// Retiring a 100 hands its INVITE's final on only where an expect of that
/// same transaction arms behind it. A CANCEL the leg has still to send stands
/// between the 100 and the INVITE's 487, and a retired expect holds that 487
/// back (§6.7c), so nothing of the transaction would take the final.
#[test]
fn retiring_a_100_arms_its_transaction_only_where_an_answer_of_it_stands_next() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        answer("s2", "A", 100, "INVITE"),
        send("s3", "A", "CANCEL"),
        answer("s4", "A", 200, "CANCEL"),
        answer("s5", "A", 487, "INVITE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert!(!cursor.retiring_arms_its_transaction("s2"), "{:?}", cursor.frontier());

    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "INVITE"),
        answer("s2", "A", 100, "INVITE"),
        answer("s3", "A", 487, "INVITE"),
        send("s4", "A", "ACK")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert!(cursor.retiring_arms_its_transaction("s2"));
    assert_eq!(cursor.frontier(), ["s2"], "the probe left the cursor as it was");
}
