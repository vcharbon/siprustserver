//! The book of unheld answers (ADR-0063 §2): a final's first arrival draws its
//! ACK and, for a 2xx, its BYE; a copy draws the ACK alone at any pace up to
//! the ladder's re-sends, then one per T1 within 64·T1 of the first arrival;
//! the rest is absorbed, and an entry outlives its last copy by 64·T1. An
//! admission dropped unsent is taken back; a full book forgets the finals
//! idle longest.

use b2bua_core::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance};
use b2bua_core::unheld_response::{UnheldAnswer, UnheldKey};
use call::{Repeated, RetainedEmission};
use sip_retransmit::Class;

use super::{UnheldBook, WINDOW_MS};

#[expect(clippy::cast_possible_wrap, reason = "a const: T1 fits an i64")]
const T1: i64 = sip_retransmit::timers::T1 as i64;

fn sent(label: &str) -> OutboundSipEffect {
    let emission = RetainedEmission::on_trigger(
        label.as_bytes().to_vec(),
        ("192.0.2.3".into(), 5070),
        Repeated::request(label),
    );
    OutboundSipEffect {
        body: OutboundBody::Datagram(emission),
        mode: OutboundTxnMode::Raw,
        destination: ("192.0.2.3".into(), 5070),
        label: label.to_string(),
        leg_id: None,
        provenance: Provenance::Authored,
    }
}

/// The answer one copy of the final `cseq` on `tag` draws: a 2xx's ACK and
/// BYE, or a non-2xx's ACK.
fn answer(tag: &str, cseq: u32, success: bool) -> UnheldAnswer {
    UnheldAnswer {
        key: UnheldKey { call_id: "unheld-call".into(), to_tag: tag.into(), cseq },
        class: if success { Class::Final2xx } else { Class::InviteServerFinal },
        ack: sent("ACK"),
        bye: success.then(|| sent("BYE")),
    }
}

/// The labels of what `sent`, one answer's admission, sends; empty when it
/// is absorbed.
fn labels(sent: Vec<Option<Vec<OutboundSipEffect>>>) -> Vec<String> {
    sent.into_iter().flatten().flatten().map(|e| e.label).collect()
}

/// The labels of what one copy arriving at `now_ms` sends at once; empty
/// when it is absorbed.
fn copy_at(book: &UnheldBook, answer: UnheldAnswer, now_ms: i64) -> Vec<String> {
    let (sent, admission) = book.admit_all(vec![answer], now_ms);
    admission.sent();
    labels(sent)
}

#[test]
fn a_2xx_is_byed_once_and_acked_on_its_ladder_then_once_per_t1() {
    let book = UnheldBook::default();
    assert_eq!(copy_at(&book, answer("t", 2, true), 0), ["ACK", "BYE"], "the first arrival");
    for copy in 1..=Class::Final2xx.resends() {
        let at = i64::from(copy);
        assert_eq!(copy_at(&book, answer("t", 2, true), at), ["ACK"], "copy {copy} at any pace");
    }
    let last = i64::from(Class::Final2xx.resends());
    assert!(copy_at(&book, answer("t", 2, true), last + 1).is_empty(), "inside T1: absorbed");
    assert_eq!(copy_at(&book, answer("t", 2, true), last + T1), ["ACK"], "T1 after the last ACK");
}

#[test]
fn a_non_2xx_is_acked_on_timer_gs_ladder_and_never_byed() {
    let book = UnheldBook::default();
    assert_eq!(copy_at(&book, answer("t", 2, false), 0), ["ACK"]);
    for copy in 1..=Class::InviteServerFinal.resends() {
        assert_eq!(copy_at(&book, answer("t", 2, false), i64::from(copy)), ["ACK"]);
    }
    let past = i64::from(Class::InviteServerFinal.resends()) + 1;
    assert!(copy_at(&book, answer("t", 2, false), past).is_empty(), "past the ladder, inside T1");
}

#[test]
fn no_copy_is_acked_64_t1_after_the_first_arrival() {
    let book = UnheldBook::default();
    copy_at(&book, answer("t", 2, true), 0);
    let mut at = 0;
    while at + T1 < WINDOW_MS {
        at += T1;
        assert_eq!(copy_at(&book, answer("t", 2, true), at), ["ACK"], "at {at} ms");
    }
    assert!(copy_at(&book, answer("t", 2, true), WINDOW_MS).is_empty(), "the window closed");
    assert!(
        copy_at(&book, answer("t", 2, true), WINDOW_MS + T1).is_empty(),
        "a copy kept inside 64·T1 of the last one keeps the entry: still absorbed",
    );
}

#[test]
fn an_entry_outlives_its_last_copy_by_64_t1() {
    let book = UnheldBook::default();
    copy_at(&book, answer("t", 2, true), 0);
    assert_eq!(copy_at(&book, answer("t", 2, true), WINDOW_MS - 1), ["ACK"], "inside the window");
    assert_eq!(
        copy_at(&book, answer("t", 2, true), 2 * WINDOW_MS - 2),
        Vec::<String>::new(),
        "64·T1 after the first arrival, inside 64·T1 of the last copy: absorbed",
    );
    assert_eq!(
        copy_at(&book, answer("t", 2, true), 3 * WINDOW_MS),
        ["ACK", "BYE"],
        "64·T1 after the last copy the entry is gone: a new first arrival",
    );
}

#[test]
fn each_final_has_its_own_entry() {
    let book = UnheldBook::default();
    assert_eq!(copy_at(&book, answer("t", 2, true), 0), ["ACK", "BYE"]);
    assert_eq!(copy_at(&book, answer("u", 2, true), 0), ["ACK", "BYE"], "another dialog");
    assert_eq!(copy_at(&book, answer("t", 3, true), 0), ["ACK", "BYE"], "another INVITE");
}

/// A turn dropped or fenced off before its answers reached the call's outbox
/// sent nothing: the next copy draws the whole answer, its BYE included.
#[test]
fn an_admission_dropped_unsent_leaves_the_book_as_it_found_it() {
    let book = UnheldBook::default();
    let (sent, admission) = book.admit_all(vec![answer("t", 2, true)], 0);
    assert_eq!(labels(sent), ["ACK", "BYE"], "the first arrival draws it all");
    drop(admission);
    assert_eq!(copy_at(&book, answer("t", 2, true), 1), ["ACK", "BYE"], "nothing left: again");
    for copy in 1..=Class::Final2xx.resends() {
        copy_at(&book, answer("t", 2, true), 1 + i64::from(copy));
    }
    let past = i64::from(Class::Final2xx.resends()) + 2;
    let (sent, admission) = book.admit_all(vec![answer("t", 2, true)], past);
    assert!(labels(sent).is_empty(), "past the ladder: absorbed");
    drop(admission);
    assert_eq!(
        copy_at(&book, answer("t", 2, true), past),
        Vec::<String>::new(),
        "a copy taken back puts the final's entry back, not a first arrival",
    );
}

/// Past its capacity the book forgets the finals idle longest: a copy of one
/// of those draws its whole answer again, one copied lately its ACK alone.
#[test]
fn a_full_book_forgets_the_finals_idle_longest() {
    let book = UnheldBook::with_capacity(4);
    for (at, tag) in ["a", "b", "c", "d"].into_iter().enumerate() {
        let at = i64::try_from(at).expect("a small index");
        assert_eq!(copy_at(&book, answer(tag, 2, true), at), ["ACK", "BYE"]);
    }
    assert_eq!(copy_at(&book, answer("a", 2, true), 10), ["ACK"], "a, copied lately");
    assert_eq!(copy_at(&book, answer("e", 2, true), 11), ["ACK", "BYE"], "a fifth: room made");
    assert_eq!(copy_at(&book, answer("d", 2, true), 12), ["ACK"], "d, kept");
    assert_eq!(copy_at(&book, answer("a", 2, true), 13), ["ACK"], "a, kept");
    assert_eq!(
        copy_at(&book, answer("b", 2, true), 14),
        ["ACK", "BYE"],
        "b, idle longest: forgotten"
    );
}
