use super::*;

pub(super) fn plan(flow: &str) -> Plan {
    let text = format!(
        r#"{{
              "pivot_version": 3,
              "case": {{ "id": "t", "title": "t", "family": "transparent", "variant": "repro",
                        "origin": "authored", "lanes": {{ "upstream-fake": "ok" }} }},
              "identities": [ {{ "name": "caller", "kind": "external-caller", "forms": ["private"] }} ],
              "calls": [ {{ "id": "c1", "caller_leg": "A", "attempts": [
                 {{ "branch": 0, "position": 0, "leg": "B", "callee": {{ "identity": "caller" }} }} ] }} ],
              "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }} ],
              "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                          {{ "id": "uas1", "kind": "uas", "endpoint": "ep0" }} ],
              "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                        {{ "id": "B", "actor": "uas1", "dir": "in" }} ],
              "flow": {flow},
              "postconditions": {{ "cdr": {{ "absent": "unit test" }} }},
              "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
            }}"#
    );
    let document = pivot_schema::PivotV3::from_json(&text).expect("the fixture parses");
    Plan::compile(document).expect("the fixture compiles")
}

pub(super) const D: &str = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;

pub(super) fn send(id: &str, leg: &str, method: &str) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"send","msg":{{"method":"{method}"}},"delay":{D}}}"#
    )
}

fn expect(id: &str, leg: &str, status: u16) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{D}}}"#
    )
}

fn racing_expect(id: &str, leg: &str, status: u16, with: &str) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","overlap":"{with}","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{D}}}"#
    )
}

/// An `expect` anchored on a step this leg does not carry: the relay of a
/// cross-leg origination, whose instant is the anchor plus a latency the
/// document does not hold.
pub(super) fn relayed(id: &str, leg: &str, status: u16, from: &str) -> String {
    let d = format!(r#"{{"ms":0,"from":"step:{from}","compressible":true,"timer_linked":false}}"#);
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{d}}}"#
    )
}

/// The same anchor with a dwell the document DOES hold: not a relay.
fn dwelling(id: &str, leg: &str, status: u16, from: &str, ms: u64) -> String {
    let d =
        format!(r#"{{"ms":{ms},"from":"step:{from}","compressible":true,"timer_linked":false}}"#);
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{d}}}"#
    )
}

fn relayed_request(id: &str, leg: &str, method: &str, from: &str) -> String {
    let d = format!(r#"{{"ms":0,"from":"step:{from}","compressible":true,"timer_linked":false}}"#);
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"method":"{method}"}},"delay":{d}}}"#
    )
}

/// An expect gated on a response to a transaction the leg names.
pub(super) fn answer(id: &str, leg: &str, status: u16, method: &str) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status},"cseq-method":"{method}"}},"delay":{D}}}"#
    )
}

/// An expect gated on a status alone, naming no transaction.
pub(super) fn status_only(id: &str, leg: &str, status: u16) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status}}},"delay":{D}}}"#
    )
}

pub(super) fn request_expect(id: &str, leg: &str, method: &str) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"method":"{method}"}},"delay":{D}}}"#
    )
}

pub(super) fn optional(id: &str, leg: &str, status: u16) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","optional":true,"check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{D}}}"#
    )
}

/// An `optional` expect of a request relayed from `from` on another leg.
fn optional_relayed_request(id: &str, leg: &str, method: &str, from: &str) -> String {
    let d = format!(r#"{{"ms":0,"from":"step:{from}","compressible":true,"timer_linked":false}}"#);
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","optional":true,"check":"record","msg":{{"method":"{method}"}},"delay":{d}}}"#
    )
}

/// An `optional` expect gated on a response to a transaction the leg names.
pub(super) fn optional_answer(id: &str, leg: &str, status: u16, method: &str) -> String {
    format!(
        r#"{{"id":"{id}","leg":"{leg}","op":"expect","optional":true,"check":"record","msg":{{"status":{status},"cseq-method":"{method}"}},"delay":{D}}}"#
    )
}

/// A declared race arms both steps at once, so the leg is not committed to
/// the order the capture happened to see.
#[test]
fn a_declared_race_arms_beside_the_step_it_names() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "BYE"),
        racing_expect("s3", "A", 200, "s2")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2", "s3"], "the race is armed together");
    // Either side may settle first: here the arrival beats the send.
    cursor.complete("s3");
    assert_eq!(cursor.frontier(), ["s2"]);
}

/// A race chain outlives its middle settling first: s3 races s2, which
/// races the send s1. Once s2 has arrived, s3 still races s1 — whatever
/// the wire settles first settles first — so it stays armed beside the send
/// rather than queueing behind it.
#[test]
fn a_race_chain_keeps_its_tail_armed_once_its_middle_settled() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "BYE"),
        racing_expect("s2", "A", 200, "s1"),
        racing_expect("s3", "A", 486, "s2")
    ));
    let mut cursor = Cursor::new(&p);
    assert_eq!(cursor.frontier(), ["s1", "s2", "s3"], "the chain arms together");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s1", "s3"], "s3 races s1 through the settled s2");
}

/// The race walks past ONE neighbour, not the whole leg.
#[test]
fn a_declared_race_does_not_arm_the_step_behind_it() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "BYE"),
        racing_expect("s2", "A", 200, "s1"),
        expect("s3", "A", 486)
    ));
    let cursor = Cursor::new(&p);
    assert_eq!(cursor.frontier(), ["s1", "s2"], "s3 still waits its turn");
}

#[test]
fn a_leg_runs_its_items_in_list_order_and_legs_run_concurrently() {
    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "INVITE"),
        expect("s2", "A", 100),
        send("s3", "B", "INVITE"),
        expect("s4", "B", 200)
    ));
    let mut cursor = Cursor::new(&p);
    assert_eq!(cursor.frontier(), ["s1", "s3"], "one head per leg");
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2", "s3"]);
    cursor.complete("s3");
    cursor.complete("s2");
    cursor.complete("s4");
    assert!(cursor.is_done());
    assert_eq!(cursor.pending(), Vec::<String>::new());
}

#[test]
fn after_holds_a_step_until_the_node_it_names_completes() {
    let mut b = format!(
        r#"{{"id":"s3","leg":"B","op":"send","after":["s2"],"msg":{{"method":"INVITE"}},"delay":{D}}}"#
    );
    b = format!("[{},{},{}]", send("s1", "A", "INVITE"), expect("s2", "A", 100), b);
    let p = plan(&b);
    let mut cursor = Cursor::new(&p);
    assert_eq!(cursor.frontier(), ["s1"], "s3 waits on s2");
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3"]);
}

#[test]
fn an_alt_arms_every_branch_and_commits_on_the_first_arrival() {
    let alt = format!(
        r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"answered","steps":[{},{}]}},
                 {{"name":"cancelled","steps":[{}]}}]}}"#,
        expect("s2", "A", 200),
        send("s3", "A", "ACK"),
        expect("s4", "A", 487)
    );
    let p = plan(&format!("[{},{}]", send("s1", "A", "INVITE"), alt));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2", "s4"], "both branch heads are armed");
    cursor.complete("s4");
    assert_eq!(cursor.committed_branch("a1"), Some("cancelled"));
    assert_eq!(cursor.status("s2"), Some(StepStatus::Discarded));
    assert_eq!(cursor.status("s3"), Some(StepStatus::Discarded));
    assert!(cursor.is_done(), "the committed branch is complete");
}

#[test]
fn a_committed_alt_runs_its_own_branch_in_order_and_never_backtracks() {
    let alt = format!(
        r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"answered","steps":[{},{}]}},
                 {{"name":"cancelled","steps":[{}]}}]}}"#,
        expect("s2", "A", 200),
        send("s3", "A", "ACK"),
        expect("s4", "A", 487)
    );
    let p = plan(&format!("[{},{}]", send("s1", "A", "INVITE"), alt));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.committed_branch("a1"), Some("answered"));
    assert_eq!(cursor.frontier(), ["s3"], "the other branch is gone");
    cursor.complete("s3");
    assert!(cursor.is_done());
}

#[test]
fn an_unordered_group_arms_every_member_and_completes_on_the_last() {
    let bye = format!(
        r#"{{"id":"s3","leg":"A","op":"expect","check":"record","msg":{{"method":"BYE"}},"delay":{D}}}"#
    );
    let group =
        format!(r#"{{"id":"u1","op":"unordered","steps":[{},{}]}}"#, expect("s2", "A", 481), bye);
    let p = plan(&format!("[{},{}]", send("s1", "A", "INVITE"), group));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2", "s3"], "order-free: both armed");
    cursor.complete("s3");
    assert_eq!(cursor.frontier(), ["s2"], "the group is not done until all arrive");
    assert!(!cursor.is_done());
    cursor.complete("s2");
    assert!(cursor.is_done());
}

#[test]
fn an_optional_expect_never_blocks_its_leg_and_is_released_by_the_step_that_overtakes_it() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "INVITE"),
        optional("s2", "A", 180),
        expect("s3", "A", 200)
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2", "s3"], "the tolerated absence does not block");
    let released = cursor.complete("s3");
    assert_eq!(released, ["s2"]);
    assert_eq!(cursor.status("s2"), Some(StepStatus::Released));
    assert!(cursor.is_done());
    assert_eq!(cursor.released(), ["s2"]);
}

#[test]
fn an_optional_expect_that_does_arrive_completes_rather_than_releasing() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "INVITE"),
        optional("s2", "A", 180),
        expect("s3", "A", 200)
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert!(cursor.complete("s2").is_empty());
    assert_eq!(cursor.status("s2"), Some(StepStatus::Complete));
    assert_eq!(cursor.frontier(), ["s3"]);
}

#[test]
fn a_send_does_not_release_a_tolerated_absence_it_overtakes() {
    // The runner controls when it sends, so a send overtaking an optional
    // expect says nothing about a message still in flight toward it.
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "INVITE"),
        optional("s2", "A", 180),
        send("s3", "A", "CANCEL")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2", "s3"]);
    assert!(cursor.complete("s3").is_empty(), "a send releases nothing");
    assert_eq!(cursor.status("s2"), Some(StepStatus::Pending));
    assert_eq!(cursor.frontier(), ["s2"], "the tolerated absence is still armed");
}

#[test]
fn only_a_tolerated_absence_may_be_released() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "INVITE"),
        optional("s2", "A", 180),
        expect("s3", "A", 200)
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert!(!cursor.release("s3"), "a required expect is never released");
    assert!(!cursor.release("s1"), "a settled step is never released");
    assert!(cursor.release("s2"));
    assert_eq!(cursor.status("s2"), Some(StepStatus::Released));
    assert!(!cursor.release("s2"), "releasing twice changes nothing");
    assert_eq!(cursor.released(), ["s2"]);
}

#[test]
fn an_incomplete_run_names_the_nodes_that_never_completed() {
    let p = plan(&format!("[{},{}]", send("s1", "A", "INVITE"), expect("s2", "A", 200)));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert!(!cursor.is_done());
    assert_eq!(cursor.pending(), ["s2"]);
}

/// A relay whose cause has already fired may be on the wire NOW, so a send
/// this leg has not made yet cannot be what orders it.
#[test]
fn an_arrival_another_leg_has_caused_arms_behind_a_send() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "A", "INVITE"),
        send("s3", "B", "INVITE"),
        relayed("s4", "B", 100, "s2"),
        relayed_request("s5", "B", "BYE", "s1")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(
        cursor.frontier(),
        ["s3", "s4", "s5"],
        "both relays are live beside the send this leg has not made yet"
    );
    // The relay beats the send: the leg takes it and its own order stands.
    cursor.complete("s5");
    assert_eq!(cursor.frontier(), ["s3", "s4"]);
}

/// A relay stands behind however many sends the leg has not made yet: the
/// walk crosses a RUN of them, not exactly one. Real-clock replay puts the
/// arrival one step earlier in the list than virtual time does, and the
/// document is the same document.
#[test]
fn an_arrival_another_leg_has_caused_arms_behind_a_run_of_sends() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "B", "ACK"),
        send("s3", "B", "INVITE"),
        relayed("s4", "B", 100, "s1"),
        relayed_request("s5", "B", "BYE", "s1")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(
        cursor.frontier(),
        ["s2", "s4", "s5"],
        "the relays live beside the FIRST send; s3 is walked past, not armed"
    );
    // The relay beats both sends: the leg takes it and its own order stands.
    cursor.complete("s5");
    assert_eq!(cursor.frontier(), ["s2", "s4"]);
}

/// Walking past a send never emits it early. The runner decides when it
/// sends, and only the leg's own head is ever armed to do so.
#[test]
fn a_send_walked_past_is_not_armed_early() {
    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "B", "ACK"),
        send("s3", "B", "INVITE"),
        relayed_request("s4", "B", "BYE", "s1")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    let frontier = cursor.frontier();
    assert!(frontier.contains(&"s4".to_string()), "the relay armed");
    assert!(!frontier.contains(&"s3".to_string()), "the send behind s2 did not");
    // s3 arms only once s2 has actually gone out.
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3", "s4"]);
}

/// The run of sends is crossed, an expect of the leg's own still is not:
/// a walk that passed sends does not thereby earn the right to pass an
/// arrival list order binds.
#[test]
fn a_run_of_sends_does_not_carry_the_walk_past_the_legs_own_expect() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "B", "ACK"),
        send("s3", "B", "INVITE"),
        expect("s4", "B", 180),
        relayed_request("s5", "B", "BYE", "s1")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2"], "s4 anchors on the trigger, so s5 waits");
}

/// The walk is a walk, not a jump: a step anchored on THIS leg is the
/// document's own order and stops it, relay or no relay behind it.
#[test]
fn a_step_anchored_on_its_own_leg_stops_the_walk() {
    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "B", "INVITE"),
        expect("s3", "B", 100),
        relayed_request("s4", "B", "BYE", "s1")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2"], "s3 anchors on the trigger, so s4 waits");
}

/// The cause is the gate. An anchor that has not fired states an arrival
/// nothing has provoked, and list order is all there is to go on.
#[test]
fn an_arrival_whose_cause_has_not_fired_waits_its_turn() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "B", "INVITE"),
        relayed_request("s3", "B", "BYE", "s1")
    ));
    let cursor = Cursor::new(&p);
    assert_eq!(cursor.frontier(), ["s1", "s2"], "s3 has nothing to wait for yet");
}

/// Only a SEND is walked past. Once the leg is waiting on an arrival of its
/// own, which of the two comes first is what list order says.
#[test]
fn an_arrival_another_leg_has_caused_waits_behind_an_expect() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "BYE"),
        expect("s2", "B", 180),
        relayed_request("s3", "B", "BYE", "s1")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2"], "the leg's own expect binds the order");
}

/// A relay arms early only where the leg can TELL IT APART: sharing a
/// discriminator with one already armed would let the earlier step's
/// datagram settle the later one.
#[test]
fn an_arrival_sharing_a_discriminator_with_one_armed_does_not_arm() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "B", "INVITE"),
        relayed("s3", "B", 200, "s1")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2", "s3"], "nothing shares its discriminator");

    let q = plan(&format!(
        "[{},{},{},{}]",
        send("t1", "A", "BYE"),
        send("t2", "B", "INVITE"),
        relayed("t3", "B", 200, "t1"),
        relayed("t4", "B", 200, "t1")
    ));
    let mut other = Cursor::new(&q);
    other.complete("t1");
    assert_eq!(
        other.frontier(),
        ["t2", "t3"],
        "t4 would take t3's datagram, so list order keeps it behind"
    );
}

/// A cross-leg anchor carrying a REAL dwell is a duration the DOCUMENT
/// holds, and §6.7a's closing rule stands for it: its order is its own.
/// Only the synthetic zero of a relay walks past a send.
#[test]
fn a_cross_leg_dwell_the_document_holds_waits_its_turn() {
    let p = plan(&format!(
        "[{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "B", "INVITE"),
        dwelling("s3", "B", 200, "s1", 700)
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2"], "700 ms is the document's own dwell");
}

/// An `alt` is never walked past. Armed early it could COMMIT before the
/// send in front of it goes out, discarding the branch that send belongs to.
#[test]
fn an_alt_behind_a_send_is_never_armed_early() {
    let alt = format!(
        r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"released","steps":[{}]}},
                 {{"name":"answered","steps":[{}]}}]}}"#,
        relayed_request("s3", "B", "BYE", "s1"),
        relayed("s4", "B", 200, "s1")
    );
    let p = plan(&format!("[{},{},{}]", send("s1", "A", "BYE"), send("s2", "B", "INVITE"), alt));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert_eq!(cursor.frontier(), ["s2"], "the alt waits for the send in front of it");
}

/// A final on a transaction retires the required expect it was charged on:
/// the step settles, what stands behind it arms, and an `after` or a dwell
/// anchored on it is satisfied.
#[test]
fn a_retired_expect_settles_and_what_stands_behind_it_arms() {
    let p = plan(&format!(
        "[{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "PRACK"),
        answer("s3", "A", 481, "PRACK"),
        send("s4", "A", "ACK")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3"]);
    assert!(cursor.retire("s3"), "a pending required expect of a message item retires");
    assert_eq!(cursor.status("s3"), Some(StepStatus::Retired));
    assert!(cursor.node_complete("s3"), "an anchor on a retired step is satisfied");
    assert_eq!(cursor.retired(), ["s3"]);
    assert_eq!(cursor.frontier(), ["s4"], "the leg moves past it");
    assert!(!cursor.retire("s3"), "retired once");
}

/// Retirement is refused on everything that is not a pending required
/// expect of a message item: a send, a completed step, an `optional` (which
/// is released instead), an `alt` branch and an `unordered` member.
#[test]
fn retirement_is_refused_outside_a_pending_required_message_expect() {
    let alt = format!(
        r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"answered","steps":[{}]}},
                 {{"name":"busy","steps":[{}]}}]}}"#,
        expect("s3", "A", 200),
        expect("s4", "A", 486)
    );
    let group = format!(
        r#"{{"id":"u1","op":"unordered","steps":[{},{}]}}"#,
        answer("s6", "A", 200, "BYE"),
        answer("s7", "A", 487, "INVITE")
    );
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        optional("s2", "A", 183),
        alt,
        send("s5", "A", "BYE"),
        group
    ));
    let mut cursor = Cursor::new(&p);
    assert!(!cursor.retire("s1"), "a send");
    cursor.complete("s1");
    assert!(!cursor.retire("s1"), "a completed step");
    assert!(!cursor.retire("s2"), "an optional is released, never retired");
    assert!(cursor.release("s2"));
    assert!(!cursor.retire("s3") && !cursor.retire("s4"), "an alt branch");
    cursor.complete("s3");
    cursor.complete("s5");
    assert_eq!(cursor.frontier(), ["s6", "s7"]);
    assert!(!cursor.retire("s6") && !cursor.retire("s7"), "an unordered member");
    assert!(cursor.retired().is_empty());
}

/// A retired final CLOSES its transaction for §6.7c: a later answer of the
/// same method behind it is another transaction's, not yet opened, and is
/// not walked to.
#[test]
fn a_required_expect_behind_a_retired_final_of_its_transaction_is_not_walked_to() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        answer("s2", "A", 486, "INVITE"),
        send("s3", "A", "ACK"),
        request_expect("s4", "A", "BYE"),
        answer("s5", "A", 200, "INVITE")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    assert!(cursor.retire("s2"));
    cursor.complete("s3");
    assert_eq!(cursor.frontier(), ["s4"], "the 200 is not the retired transaction's answer");
}

/// An answer armed beside a REQUIRED expect still pending (§6.7c) releases
/// no tolerated absence standing between the two: their order against the
/// answer is unknown, and the INVITE's 180 behind its pending 183 may still
/// come.
#[test]
fn an_answer_armed_beside_a_pending_expect_releases_no_optional_behind_it() {
    let p = plan(&format!(
        "[{},{},{},{},{}]",
        send("s1", "A", "INVITE"),
        send("s2", "A", "INFO"),
        answer("s3", "A", 183, "INVITE"),
        optional("s4", "A", 180),
        answer("s5", "A", 200, "INFO")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3", "s5"], "the INFO's answer arms beside the 183");
    assert!(cursor.complete("s5").is_empty(), "nothing released past the pending 183");
    assert_eq!(cursor.status("s4"), Some(StepStatus::Pending));
    assert!(cursor.complete("s3").is_empty());
    assert_eq!(cursor.frontier(), ["s4"], "the tolerated 180 is armed in its turn");
    cursor.complete("s4");
    assert_eq!(cursor.status("s4"), Some(StepStatus::Complete));
    assert!(cursor.is_done());
}

/// A relay armed early behind a send this leg has not made (§6.7b) releases
/// no tolerated absence standing behind that send: the send has not gone
/// out, so nothing says the optional's message will not still come.
/// Optionals standing before the pending send are released as ever.
#[test]
fn a_relay_armed_past_a_pending_send_releases_no_optional_behind_it() {
    let p = plan(&format!(
        "[{},{},{},{},{},{}]",
        send("s1", "A", "BYE"),
        send("s2", "B", "INVITE"),
        optional("s3", "B", 100),
        send("s4", "B", "ACK"),
        optional_relayed_request("s5", "B", "INFO", "s1"),
        relayed_request("s6", "B", "BYE", "s1")
    ));
    let mut cursor = Cursor::new(&p);
    cursor.complete("s1");
    cursor.complete("s2");
    assert_eq!(cursor.frontier(), ["s3", "s4", "s5", "s6"]);
    assert_eq!(cursor.complete("s6"), ["s3"], "the optional before the send is released");
    assert_eq!(cursor.status("s5"), Some(StepStatus::Pending), "the one behind it is not");
    cursor.complete("s4");
    assert_eq!(cursor.frontier(), ["s5"]);
}
