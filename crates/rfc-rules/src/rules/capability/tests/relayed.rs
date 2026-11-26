//! `options-response-echoes` on a relaying element: a bare 2xx carried back
//! from a far UAS that answered bare is relayed, never the element's own miss.

use super::*;

/// The UAS bob asks onward, on a call of its own.
const FAR: &str = "127.0.0.1:5080";

/// bob's OPTIONS sent onward to the far UAS at `asked_us`, and the 2xx bob
/// takes back from it at `answered_us`, stating `extra`.
fn onward(asked_us: u64, answered_us: u64, extra: &str) -> [Msg; 2] {
    onward_on("z9hG4bK-f", asked_us, answered_us, extra)
}

/// As [`onward`], on the transaction `branch`.
fn onward_on(branch: &str, asked_us: u64, answered_us: u64, extra: &str) -> [Msg; 2] {
    let far_leg = |m: Msg, src: &str, dst: &str| Msg {
        src: src.to_string(),
        dst: dst.to_string(),
        call_id: "c2".to_string(),
        ..m
    };
    [
        far_leg(taken(asked_us, "OPTIONS", branch, 7, ""), BOB, FAR),
        far_leg(answered(answered_us, 200, "OPTIONS", branch, 7, extra), FAR, BOB),
    ]
}

/// The violated finding `f` charges to `emitter`.
fn charged<'f>(f: &'f [Finding], emitter: &str) -> &'f Finding {
    let mut of = f.iter().filter(|x| x.emitter == emitter);
    let found = of.next().unwrap_or_else(|| panic!("nothing charged to {emitter}: {f:?}"));
    assert!(of.next().is_none(), "{emitter} charged twice: {f:?}");
    found
}

/// bob takes alice's OPTIONS, asks the far UAS, and relays its bare 2xx
/// bare: the miss is the far UAS's, and bob's copy is marked relayed.
#[test]
fn a_bare_options_2xx_relayed_from_a_bare_far_answer_is_relayed() {
    let [ask, far] = onward(1_500, 2_000, "");
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        ask,
        far,
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    assert!(!charged(&f, FAR).relayed, "the far UAS answered bare on its own account");
    assert!(charged(&f, BOB).relayed, "bob carries back the far UAS's bare answer");
}

/// The far UAS described itself and bob's 2xx carries none of it: bob
/// dropped what it took, and the miss is his.
#[test]
fn a_relay_that_drops_the_far_description_is_charged() {
    let [ask, far] = onward(1_500, 2_000, "Allow: INVITE\r\n");
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        ask,
        far,
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    assert!(!charged(&f, BOB).relayed, "{f:?}");
}

/// A bare far 2xx to an OPTIONS bob sent before taking alice's answers
/// another question: bob's bare 2xx is his own.
#[test]
fn a_far_answer_to_an_earlier_options_relays_nothing() {
    let [ask, far] = onward(500, 800, "");
    let msgs = [
        ask,
        far,
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    assert!(!charged(&f, BOB).relayed, "{f:?}");
}

/// A bare far 2xx bob takes only after its own 2xx left is not what it
/// carried back.
#[test]
fn a_far_answer_taken_after_the_answer_left_relays_nothing() {
    let [ask, far] = onward(1_500, 2_500, "");
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        ask,
        answered(2_000, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
        far,
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    assert!(!charged(&f, BOB).relayed, "{f:?}");
}

/// One bare far 2xx is carried back once: of two bare 2xx bob answers, one
/// relays it and the other is his own.
#[test]
fn one_far_answer_relays_one_answer() {
    let [ask, far] = onward(1_500, 2_000, "");
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        taken(1_100, "OPTIONS", "z9hG4bK-p", 2, ""),
        ask,
        far,
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
        answered(2_600, 200, "OPTIONS", "z9hG4bK-p", 2, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    let bob: Vec<bool> = f.iter().filter(|x| x.emitter == BOB).map(|x| x.relayed).collect();
    assert_eq!(bob, [true, false], "{f:?}");
}

/// A far 2xx retransmitted is one answer: it carries back to one of bob's
/// two bare 2xx, never both.
#[test]
fn a_retransmitted_far_answer_relays_one_answer() {
    let [ask, far] = onward(1_500, 2_000, "");
    let far_again = Msg { at_us: 2_050, repeat: true, ..far.clone() };
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        taken(1_100, "OPTIONS", "z9hG4bK-p", 2, ""),
        ask,
        far,
        far_again,
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
        answered(2_600, 200, "OPTIONS", "z9hG4bK-p", 2, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    let bob: Vec<bool> = f.iter().filter(|x| x.emitter == BOB).map(|x| x.relayed).collect();
    assert_eq!(bob, [true, false], "{f:?}");
}

/// bob's own bare answer to one of alice's OPTIONS is no far answer to the
/// other: with nothing asked onward, both bare 2xx are his.
#[test]
fn an_answer_bob_sent_is_no_far_answer() {
    let msgs = [
        taken(900, "OPTIONS", "z9hG4bK-p", 2, ""),
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        answered(2_000, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
        answered(2_100, 200, "OPTIONS", "z9hG4bK-p", 2, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f.iter().all(|x| !x.relayed), "{f:?}");
}

/// alice's OPTIONS retransmitted after bob asked onward is still the one bob
/// took first: the far 2xx answers it, and bob's bare 2xx relays it.
#[test]
fn a_retransmitted_options_keeps_the_first_take() {
    let [ask, far] = onward(1_500, 2_000, "");
    let again = Msg { repeat: true, ..taken(1_600, "OPTIONS", "z9hG4bK-o", 1, "") };
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        ask,
        again,
        far,
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    assert!(charged(&f, BOB).relayed, "{f:?}");
}

/// A described 2xx relayed from a bare far one meets §11.2: compliant, and
/// nothing to mark.
#[test]
fn a_described_answer_to_a_bare_far_answer_is_compliant() {
    let [ask, far] = onward(1_500, 2_000, "");
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        ask,
        far,
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, "Allow: INVITE\r\n"),
    ];
    let all = run(&OptionsResponseEchoes, &msgs);
    let bob = all.iter().find(|x| x.emitter == BOB).expect("bob's occasion");
    assert!(matches!(bob.decision, Decision::Compliant), "{all:?}");
    assert!(!bob.relayed, "{all:?}");
}

/// bob takes a described far 2xx, then a bare one: the first far answer is
/// the one carried back (§16.7 forwards a 2xx as it arrives), so bob's bare
/// 2xx dropped its description.
#[test]
fn the_first_far_answer_is_the_one_carried_back() {
    let [ask_f, described] = onward_on("z9hG4bK-f", 1_500, 1_800, "Allow: INVITE\r\n");
    let [ask_g, bare] = onward_on("z9hG4bK-g", 1_600, 2_000, "");
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        ask_f,
        ask_g,
        described,
        bare,
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    assert!(!charged(&f, BOB).relayed, "{f:?}");
}

/// Two OPTIONS asked onward in the order bob took them are answered in that
/// order: each of bob's 2xx carries back the far answer to its own question,
/// the bare one relayed, the one that dropped a description charged.
#[test]
fn interleaved_far_answers_pair_first_in_first_out() {
    let [ask_f, bare] = onward_on("z9hG4bK-f", 1_500, 1_800, "");
    let [ask_g, described] = onward_on("z9hG4bK-g", 1_600, 2_000, "Allow: INVITE\r\n");
    let msgs = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        taken(1_100, "OPTIONS", "z9hG4bK-p", 2, ""),
        ask_f,
        ask_g,
        bare,
        described,
        answered(2_500, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
        answered(2_600, 200, "OPTIONS", "z9hG4bK-p", 2, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &msgs);
    let bob: Vec<bool> = f.iter().filter(|x| x.emitter == BOB).map(|x| x.relayed).collect();
    assert_eq!(bob, [true, false], "{f:?}");
}
