use std::time::Duration;

use tokio::time::Instant;

use super::*;

fn budgets(messages: u64, entries: usize) -> Budgets {
    Budgets::new(CallIdBudget { messages, entries })
}

fn tripped(admission: Admission) -> CallIdBudgetTrip {
    match admission {
        Admission::Tripped(trip) => trip,
        other => panic!("a trip, not {other:?}"),
    }
}

#[test]
fn the_datagram_past_the_budget_trips_and_every_later_one_is_dropped() {
    let t0 = Instant::now();
    let mut b = budgets(3, 16);
    for _ in 0..3 {
        assert_eq!(b.admit("c1", t0), Admission::Counted);
    }
    let trip = tripped(b.admit("c1", t0));
    assert_eq!(trip.call_id, "c1");
    assert_eq!(b.admit("c1", t0), Admission::Muted, "a muted Call-ID drops");
    assert_eq!(b.admit("c2", t0), Admission::Counted, "another Call-ID counts apart");
}

#[test]
fn a_trip_names_the_callers_and_calls_of_its_transactions_at_most_a_few() {
    let t0 = Instant::now();
    let mut b = budgets(1, 16);
    b.opened("c1", None, Some("alice"), t0);
    b.opened("c1", Some("call-1"), None, t0);
    b.opened("c1", Some("call-1"), Some("alice"), t0);
    for n in 2..=6 {
        b.opened("c1", Some(&format!("call-{n}")), None, t0);
    }
    b.admit("c1", t0);
    let trip = tripped(b.admit("c1", t0));
    assert_eq!(trip.caller_tags, ["alice"]);
    assert_eq!(trip.call_refs, ["call-1", "call-2", "call-3", "call-4"]);
}

#[test]
fn every_drop_restarts_a_mutes_64_t1() {
    let t0 = Instant::now();
    let mut b = budgets(1, 16);
    b.admit("c1", t0);
    tripped(b.admit("c1", t0));
    let drop_at = t0 + Duration::from_secs(20);
    assert_eq!(b.admit("c1", drop_at), Admission::Muted);
    let past_trip = t0 + WINDOW + Duration::from_secs(1);
    assert_eq!(b.admit("c1", past_trip), Admission::Muted, "the drop at 20 s restarted it");
    let just_short = past_trip + WINDOW - Duration::from_millis(1);
    assert_eq!(b.admit("c1", just_short), Admission::Muted);
    let lifted = just_short + WINDOW;
    assert_eq!(b.admit("c1", lifted), Admission::Counted, "64·T1 after the last drop it lifts");
}

#[test]
fn a_live_transaction_keeps_a_mute_past_its_window() {
    let t0 = Instant::now();
    let mut b = budgets(1, 16);
    b.opened("c1", None, None, t0);
    b.admit("c1", t0);
    tripped(b.admit("c1", t0));
    b.expire(t0 + WINDOW * 3);
    assert_eq!(b.admit("c1", t0 + WINDOW * 3), Admission::Muted);
    b.closed("c1");
    let later = t0 + WINDOW * 5;
    b.expire(later);
    assert_eq!(b.admit("c1", later), Admission::Counted, "the sweep lifted it");
}

#[test]
fn an_idle_count_with_no_transaction_lapses_after_64_t1() {
    let t0 = Instant::now();
    let mut b = budgets(2, 16);
    b.admit("c1", t0);
    b.admit("c1", t0);
    tripped(b.admit("c1", t0 + Duration::from_secs(1)));
    b.admit("c2", t0);
    b.admit("c2", t0);
    let lapsed = t0 + WINDOW;
    assert_eq!(b.admit("c2", lapsed), Admission::Counted, "c2 counts afresh");
    b.opened("c3", None, None, t0);
    b.admit("c3", t0);
    b.admit("c3", t0);
    tripped(b.admit("c3", lapsed));
}

#[test]
fn a_full_map_evicts_an_unmuted_count_never_a_mute() {
    let t0 = Instant::now();
    let mut b = budgets(1, 2);
    b.admit("muted", t0);
    tripped(b.admit("muted", t0));
    b.admit("live", t0);
    assert_eq!(b.admit("new", t0), Admission::Counted, "the unmuted count makes room");
    assert_eq!(b.admit("muted", t0), Admission::Muted, "the mute stays");
    tripped(b.admit("new", t0));
    assert_eq!(b.admit("newer", t0), Admission::Uncounted, "a map of mutes counts no newcomer");
    assert_eq!(b.admit("newer", t0), Admission::Uncounted);
}

#[test]
fn a_full_map_keeps_a_count_whose_transaction_lives() {
    let t0 = Instant::now();
    let mut b = budgets(2, 1);
    b.opened("live", None, Some("alice"), t0);
    assert_eq!(b.admit("stray", t0), Admission::Uncounted, "the live count is not evicted");
    b.admit("live", t0);
    b.admit("live", t0);
    let trip = tripped(b.admit("live", t0));
    assert_eq!(trip.caller_tags, ["alice"], "the live INVITE's caller is still named");
    b.expire(t0 + WINDOW * 3);
    assert_eq!(
        b.admit("live", t0 + WINDOW * 3),
        Admission::Muted,
        "its transaction holds the mute"
    );
}

#[test]
fn a_transaction_opened_on_a_map_full_of_mutes_is_counted() {
    let t0 = Instant::now();
    let mut b = budgets(1, 1);
    b.admit("muted", t0);
    tripped(b.admit("muted", t0));
    b.opened("call", None, Some("bob"), t0);
    assert_eq!(b.admit("call", t0), Admission::Counted);
    let trip = tripped(b.admit("call", t0));
    assert_eq!(trip.caller_tags, ["bob"]);
    assert_eq!(b.admit("muted", t0), Admission::Muted, "the mute stays");
}

#[test]
fn a_full_map_evicts_the_count_that_went_idle_first() {
    let t0 = Instant::now();
    let mut b = budgets(2, 2);
    b.admit("old", t0);
    b.admit("recent", t0 + Duration::from_secs(1));
    assert_eq!(b.admit("new", t0 + Duration::from_secs(2)), Admission::Counted);
    assert_eq!(b.admit("recent", t0 + Duration::from_secs(2)), Admission::Counted);
    tripped(b.admit("recent", t0 + Duration::from_secs(2)));
}

#[test]
fn a_count_back_to_idle_queues_behind_the_counts_idle_before_it() {
    let t0 = Instant::now();
    let mut b = budgets(2, 2);
    b.admit("first", t0);
    b.admit("second", t0);
    b.opened("first", None, None, t0);
    b.closed("first");
    assert_eq!(b.admit("new", t0), Admission::Counted, "\"second\" gave its place");
    assert_eq!(b.admit("first", t0), Admission::Counted);
    tripped(b.admit("first", t0));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a transaction closed under a Call-ID counting none")]
fn closing_a_transaction_never_opened_is_a_defect() {
    budgets(1, 16).closed("c1");
}
