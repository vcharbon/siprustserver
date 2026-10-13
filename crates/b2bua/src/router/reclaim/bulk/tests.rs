//! The re-anchor pass of a bulk reclaim: the largest past-due keepalive gap,
//! read call by call, each call one beat of the router's pulse.

use std::sync::atomic::{AtomicUsize, Ordering};

use call::{TimerEntry, TimerKey, TimerType};

use super::largest_keepalive_gap;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, src};

fn call_with_keepalive_at(cid: &str, fire_at: i64) -> call::Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call =
        build_initial_call(&invite("w0", "w1", cid), src(), &config, &sip_txn::IdGen::seeded(1), 0);
    call.timers.push(TimerEntry {
        key: TimerKey::call(TimerType::Keepalive),
        generation: 1,
        fire_at,
    });
    call
}

#[test]
fn the_pass_beats_once_per_call_and_reads_the_largest_gap() {
    let mut calls = vec![
        (call_with_keepalive_at("a", 9_000), 0),
        (call_with_keepalive_at("b", 4_000), 0),
        (call_with_keepalive_at("c", 12_000), 0),
    ];
    let beats = AtomicUsize::new(0);
    let gap = largest_keepalive_gap(&mut calls, 10_000, &|| {
        beats.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(gap, 6_000, "the most overdue keepalive, 6 s past due");
    assert_eq!(beats.load(Ordering::SeqCst), 3);
}
