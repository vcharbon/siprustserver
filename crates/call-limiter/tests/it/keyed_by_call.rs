//! Admission holds keyed by the call.
//!
//! The store keeps, per limiter key, the entries the call holds, the change
//! number of its last admit and a lease. `admit(key, change, entries,
//! release_on_refusal)` replaces the call's set atomically, checked net of the
//! call's own set, and is superseded when not above the number held;
//! `release(keys)` is idempotent per key and fences each key for one lease;
//! `refresh(key, change, entries)` extends the lease or re-registers a lapsed
//! set; every admit and refresh answer states the set held; a set whose lease
//! lapses is dropped and counted.
//!
//! Every scenario carries a **witness** hold on each id, admitted for a call
//! of its own, so a surplus release reads below the witness instead of
//! vanishing under the floor at 0.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use call_limiter::wire::{
    AdmitAnswer, AdmitEntry, AdmitRequest, AdmitResponse, HeldSet, RefreshAnswer, RefreshCall,
    RefreshRequest, RefreshResponse, ReleaseRequest,
};
use call_limiter::{
    AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer, RefreshResult,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;

/// A short lease so the paused clock crosses it cheaply.
const LEASE: Duration = Duration::from_secs(10);

fn store() -> CallStore {
    CallStore::new(LimiterConfig { lease_sec: LEASE.as_secs() as i64 }, Clock::test_at(0))
}

fn entry(id: &str, limit: i64) -> AdmitEntry {
    AdmitEntry { id: id.into(), limit }
}

fn entries(ids: &[(&str, i64)]) -> Vec<AdmitEntry> {
    ids.iter().map(|(id, limit)| entry(id, *limit)).collect()
}

/// One witness hold per id, each under its own call.
fn witnesses(s: &CallStore, ids: &[&str]) {
    for id in ids {
        let r = s.admit(&format!("witness-{id}"), n(), &[entry(id, 100)], false);
        assert_eq!(r, AdmitResult::Admitted, "witness on {id}");
    }
}

/// The next change number: above every one handed out before, so an admit
/// is never superseded unless a scenario says so.
fn n() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The id a cap refusal names; panics on any other outcome.
#[track_caller]
fn refused_on(r: AdmitResult) -> String {
    match r {
        AdmitResult::Rejected { limiter_id, .. } => limiter_id,
        other => panic!("expected a cap refusal, got {other:?}"),
    }
}

/// `ids`, each at cap 10: a set a refresh re-registers.
fn ids(ids: &[&str]) -> Vec<AdmitEntry> {
    ids.iter().map(|id| entry(id, 10)).collect()
}

/// The outcome of each call a refresh answer names, in order.
fn outcomes(body: &RefreshResponse) -> Vec<RefreshAnswer> {
    body.outcomes.iter().map(|reply| reply.outcome).collect()
}

async fn advance(d: Duration) {
    tokio::time::advance(d).await;
}

// ── net of the call's own set ─────────────────────────────────────────

/// `[x, y]` → `[y, z]` with `y` at its cap: the call keeps `y`, so `y`
/// needs no new slot and the replacement is admitted.
#[tokio::test(start_paused = true)]
async fn a_replacement_keeping_an_id_at_its_cap_is_admitted() {
    let s = store();
    witnesses(&s, &["x", "y", "z"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    // y: the witness + the call = 2 = the cap.
    assert_eq!(s.admit("c1", n(), &entries(&[("y", 2), ("z", 10)]), false), AdmitResult::Admitted);
    assert_eq!(
        [s.held("x"), s.held("y"), s.held("z")],
        [1, 2, 2],
        "x is back to its witness, y is held once by the call, z once"
    );
}

/// `[x, x]` → `[x]` with `x` at its cap: the call reduces `x`, which never
/// refuses.
#[tokio::test(start_paused = true)]
async fn a_replacement_reducing_an_id_at_its_cap_is_admitted() {
    let s = store();
    witnesses(&s, &["x"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10), ("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 3);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 2)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 2, "the witness and one hold of the call");
}

/// An id the call adds is checked against the cap: `[x]` → `[x, y]` with
/// `y` at its cap is refused on `y`, and nothing moves.
#[tokio::test(start_paused = true)]
async fn a_replacement_adding_an_id_at_its_cap_is_refused() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(refused_on(s.admit("c1", n(), &entries(&[("x", 10), ("y", 1)]), false)), "y");
    assert_eq!([s.held("x"), s.held("y")], [2, 1], "the old set stays, y untouched");
}

/// The same id twice in one list takes two slots.
#[tokio::test(start_paused = true)]
async fn the_same_id_twice_on_one_list_takes_two_slots() {
    let s = store();
    witnesses(&s, &["x"]);
    // Room for one more only: the second slot is refused, nothing counted.
    assert_eq!(refused_on(s.admit("c1", n(), &entries(&[("x", 2), ("x", 2)]), false)), "x");
    assert_eq!(s.held("x"), 1);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 3), ("x", 3)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 3);
    s.release(&["c1"]);
    assert_eq!(s.held("x"), 1, "both slots released at once");
}

/// A refusal on the second entry increments nothing, not even the first,
/// and keeps the call's old set when `release_on_refusal` is off.
#[tokio::test(start_paused = true)]
async fn a_refusal_on_the_second_entry_keeps_the_old_set_and_counts_nothing() {
    let s = store();
    witnesses(&s, &["w", "x", "y"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("w", 10)]), false), AdmitResult::Admitted);
    assert_eq!(refused_on(s.admit("c1", n(), &entries(&[("x", 10), ("y", 1)]), false)), "y");
    assert_eq!([s.held("w"), s.held("x"), s.held("y")], [2, 1, 1]);
}

/// With `release_on_refusal`, a refused replacement drops the old set in
/// the same step: the call holds nothing, the witnesses are intact.
#[tokio::test(start_paused = true)]
async fn a_refused_replacement_with_release_on_refusal_drops_the_old_set() {
    let s = store();
    witnesses(&s, &["w", "x", "y"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("w", 10), ("w", 10)]), false), AdmitResult::Admitted);
    assert_eq!(refused_on(s.admit("c1", n(), &entries(&[("x", 10), ("y", 1)]), true)), "y");
    assert_eq!([s.held("w"), s.held("x"), s.held("y")], [1, 1, 1], "only the witnesses");
    assert_eq!(s.calls(), 3, "the refused call holds no set");
    // A later admit of the same call is an ordinary admit (the drop fences
    // refresh only: the call was not released, its replacement was refused).
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 2);
}

/// An empty list replaces the call's set with nothing: the holds are freed,
/// the key is fenced against refresh (a drop fence, not a release), and a
/// later admit of the call is ordinary and clears the fence.
#[tokio::test(start_paused = true)]
async fn an_empty_replacement_frees_the_set_behind_a_drop_fence() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c1", n(), &[], true), AdmitResult::Admitted);
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "only the witnesses");
    assert_eq!(s.calls(), 2, "the call holds no set");
    assert_eq!(s.stats().fences, 1, "the drop fences the key");
    assert_eq!(
        s.refresh("c1", 0, &ids(&["x", "y"])).result,
        RefreshResult::Dropped,
        "a refresh re-creates nothing behind the drop fence, and says why"
    );
    assert_eq!(s.admit("c1", n(), &entries(&[("y", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("y"), 2);
    assert_eq!(s.stats().fences, 0, "the admit cleared the fence");
}

/// Each admit is checked against its own entries' caps: the store keeps no
/// cap per id.
#[tokio::test(start_paused = true)]
async fn each_admit_is_checked_against_its_own_entry_cap() {
    let s = store();
    witnesses(&s, &["x"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 2)]), false), AdmitResult::Admitted);
    // c2 states a higher cap: admitted under it.
    assert_eq!(s.admit("c2", n(), &entries(&[("x", 3)]), false), AdmitResult::Admitted);
    assert_eq!(refused_on(s.admit("c3", n(), &entries(&[("x", 3)]), false)), "x");
}

/// An id the call keeps or reduces is never checked: `[x(10)]` → `[x(1)]`
/// with two witnesses on `x` is admitted although `x` is over the new cap.
#[tokio::test(start_paused = true)]
async fn a_kept_id_over_a_lower_cap_is_admitted() {
    let s = store();
    witnesses(&s, &["x"]);
    assert_eq!(s.admit("witness-x-2", n(), &entries(&[("x", 100)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 3);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 1)]), true), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 3, "the call keeps its one hold on x");
}

/// `admission_max` is the largest live count over the ids, what the next
/// admit of that id compares with its cap; 0 once everything is released.
#[tokio::test(start_paused = true)]
async fn the_admission_max_gauge_is_the_largest_live_count() {
    let s = store();
    assert_eq!(s.stats().admission_max, 0);
    witnesses(&s, &["x", "y"]);
    assert_eq!(
        s.admit("c1", n(), &entries(&[("x", 10), ("x", 10), ("y", 10)]), false),
        AdmitResult::Admitted
    );
    assert_eq!(s.stats().admission_max, 3, "x is held three times");
    s.release(&["c1"]);
    assert_eq!(s.stats().admission_max, 1, "the witnesses");
    s.release(&["witness-x"]);
    s.release(&["witness-y"]);
    assert_eq!(s.stats().admission_max, 0);
}

// ── release ───────────────────────────────────────────────────────────

/// A second release of one call is a no-op; a release of an unknown call
/// is a no-op: the witnesses are intact.
#[tokio::test(start_paused = true)]
async fn release_is_idempotent_and_unknown_calls_are_a_no_op() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    s.release(&["c1"]);
    s.release(&["c1"]);
    s.release(&["never-admitted"]);
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "the witnesses");
    assert_eq!(s.stats().current_total, 2);
}

/// One release names several calls: each named call's set is dropped and
/// its key fenced; a key named twice, or unknown, changes nothing more.
#[tokio::test(start_paused = true)]
async fn one_release_frees_every_call_it_names() {
    let s = store();
    witnesses(&s, &["x", "y", "z"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c2", n(), &entries(&[("y", 10), ("z", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c3", n(), &entries(&[("x", 10), ("z", 10)]), false), AdmitResult::Admitted);
    s.release(&["c1", "c2", "c1", "never-admitted"]);
    assert_eq!([s.held("x"), s.held("y"), s.held("z")], [2, 1, 2], "c3 and the witnesses");
    assert_eq!(s.calls(), 4, "c3 and the witnesses");
    assert_eq!(s.stats().fences, 3, "c1, c2 and the unknown key are fenced");
    assert_eq!(s.stats().releases_total, 4, "one per key named");
    assert_eq!(
        s.admit("c2", n(), &entries(&[("y", 10)]), false),
        AdmitResult::Released,
        "an admit after the release meets the fence"
    );
}

/// A release of a key the store holds nothing for changes no count and
/// creates no set, and fences the key for one lease: an admit or a refresh
/// landing after it is refused and creates nothing.
#[tokio::test(start_paused = true)]
async fn a_release_of_an_unknown_key_creates_nothing_and_fences_the_key() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    s.release(&["c1"]);
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "the witnesses");
    assert_eq!(s.calls(), 2, "no set created");
    assert_eq!(s.stats().fences, 1, "the key is fenced");
    assert_eq!(
        s.admit("c1", n(), &entries(&[("x", 10), ("y", 10)]), false),
        AdmitResult::Released,
        "an admit landing after the release is refused by its fence"
    );
    assert_eq!(s.refresh("c1", 0, &ids(&["x"])).result, RefreshResult::Released);
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "the witnesses");
    assert_eq!(s.calls(), 2, "no set created");
}

/// A released call is fenced for one lease: an admit is refused with a
/// distinct reason (not a cap refusal) and holds nothing; a refresh says
/// the call is unknown.
#[tokio::test(start_paused = true)]
async fn a_released_call_refuses_admit_and_refresh_for_one_lease() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    s.release(&["c1"]);
    assert_eq!(
        s.admit("c1", n(), &entries(&[("y", 10)]), false),
        AdmitResult::Released,
        "a fold landing after the call ended holds nothing"
    );
    assert_eq!(s.held("y"), 1);
    assert_eq!(
        s.refresh("c1", 0, &ids(&["x"])).result,
        RefreshResult::Released,
        "a refresh of a released call re-creates nothing"
    );
    assert_eq!(s.held("x"), 1);
    s.release(&["c1"]);
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "a second release is a no-op");
}

/// A release fence expires after one lease: the same key is then an
/// ordinary new call.
#[tokio::test(start_paused = true)]
async fn a_release_fence_expires_after_one_lease() {
    let s = store();
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    s.release(&["c1"]);
    advance(LEASE - Duration::from_secs(1)).await;
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Released);
    advance(Duration::from_secs(2)).await;
    s.sweep_now();
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 1);
    s.release(&["c1"]);
}

/// An admit that dropped the set without replacing it (a cap refusal with
/// `release_on_refusal`, an empty replacement) fences the key against a
/// refresh landing after it: nothing is re-created, and the answer is
/// `Dropped`, not the release fence's `Released`. The next admit of the key
/// clears the fence, and a refresh then extends.
#[tokio::test(start_paused = true)]
async fn a_refresh_after_an_admit_dropped_the_set_re_creates_nothing() {
    let s = store();
    witnesses(&s, &["x", "z"]);
    // Dropped by a refused replacement.
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(refused_on(s.admit("c1", n(), &entries(&[("y", 10), ("z", 1)]), true)), "z");
    assert_eq!(
        s.refresh("c1", 0, &ids(&["x"])).result,
        RefreshResult::Dropped,
        "fenced by the drop"
    );
    assert_eq!(s.held("x"), 1, "the witness only");
    // Dropped by an empty replacement.
    assert_eq!(s.admit("c2", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c2", n(), &[], true), AdmitResult::Admitted);
    assert_eq!(
        s.refresh("c2", 0, &ids(&["x"])).result,
        RefreshResult::Dropped,
        "fenced by the drop"
    );
    assert_eq!(s.held("x"), 1);
    assert_eq!(s.stats().reregistered_calls, 0);
    // A later admit of the key clears the fence.
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.refresh("c1", 0, &ids(&["x"])).result, RefreshResult::Extended);
    assert_eq!(s.held("x"), 2);
    // The drop fence lapses with a lease, like a release fence.
    advance(LEASE + Duration::from_secs(1)).await;
    s.sweep_now();
    assert_eq!(s.refresh("c2", 0, &ids(&["x"])).result, RefreshResult::Reregistered);
}

/// A refresh carrying no ids for an unknown key registers nothing and counts
/// nothing.
#[tokio::test(start_paused = true)]
async fn a_refresh_without_ids_of_an_unknown_call_is_a_no_op() {
    let s = store();
    assert_eq!(s.refresh("never-admitted", 0, &ids(&[])).result, RefreshResult::Released);
    assert_eq!(s.calls(), 0);
    assert_eq!(s.stats().reregistered_calls, 0);
}

// ── lease ─────────────────────────────────────────────────────────────

/// A set whose lease lapses is dropped by the sweep and counted: one
/// expired call, its holds.
#[tokio::test(start_paused = true)]
async fn lease_expiry_drops_the_set_and_counts_it() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(
        s.admit("c1", n(), &entries(&[("x", 10), ("x", 10), ("y", 10)]), false),
        AdmitResult::Admitted
    );
    // Keep the witnesses alive across the lease.
    advance(LEASE / 2).await;
    assert_eq!(s.refresh("witness-x", 0, &ids(&["x"])).result, RefreshResult::Extended);
    assert_eq!(s.refresh("witness-y", 0, &ids(&["y"])).result, RefreshResult::Extended);
    advance(LEASE / 2 + Duration::from_secs(1)).await;
    assert_eq!(s.sweep_now(), 1, "one call expired");
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "only the witnesses");
    let stats = s.stats();
    assert_eq!(stats.lease_expired_calls, 1);
    assert_eq!(stats.lease_expired_holds, 3);
    assert_eq!(stats.current_total, 2);
}

/// A refresh extends the lease; a replace resets it.
#[tokio::test(start_paused = true)]
async fn refresh_and_replace_extend_the_lease() {
    let s = store();
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c2", n(), &entries(&[("y", 10)]), false), AdmitResult::Admitted);
    advance(LEASE - Duration::from_secs(1)).await;
    assert_eq!(s.refresh("c1", 0, &ids(&["x"])).result, RefreshResult::Extended);
    assert_eq!(s.admit("c2", n(), &entries(&[("y", 10), ("z", 10)]), false), AdmitResult::Admitted);
    advance(Duration::from_secs(2)).await;
    assert_eq!(s.sweep_now(), 0, "both leases were extended");
    assert_eq!([s.held("x"), s.held("y"), s.held("z")], [1, 1, 1]);
    advance(LEASE).await;
    assert_eq!(s.sweep_now(), 2);
    assert_eq!(s.stats().current_total, 0);
}

/// A refresh of a call the store does not know and has not fenced
/// re-creates its set from the ids it carries, with no cap check: the call
/// exists and was admitted.
#[tokio::test(start_paused = true)]
async fn a_refresh_of_an_unknown_call_re_registers_its_set_without_a_cap_check() {
    let s = store();
    witnesses(&s, &["x"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    advance(LEASE + Duration::from_secs(1)).await;
    assert_eq!(s.sweep_now(), 2, "the witness and the call lapsed");
    assert_eq!([s.held("x"), s.held("y")], [0, 0]);
    // x at cap 1 would refuse an admit; the refresh re-registers regardless.
    assert_eq!(s.admit("filler", n(), &entries(&[("x", 1)]), false), AdmitResult::Admitted);
    assert_eq!(s.refresh("c1", 0, &ids(&["x", "x", "y"])).result, RefreshResult::Reregistered);
    assert_eq!([s.held("x"), s.held("y")], [3, 1], "the set the refresh carries, cap or not");
    assert_eq!(s.stats().reregistered_calls, 1);
    assert_eq!(s.refresh("c1", 0, &ids(&["x"])).result, RefreshResult::Extended, "known again");
    s.release(&["c1"]);
    assert_eq!([s.held("x"), s.held("y")], [1, 0], "one release frees the re-registered set");
}

/// A refresh and the call's release in either order: after both, the store
/// holds nothing for the call.
#[tokio::test(start_paused = true)]
async fn a_refresh_before_or_after_the_release_leaves_nothing_held() {
    let s = store();
    // Release first: the release fence refuses the refresh.
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    s.release(&["c1"]);
    assert_eq!(s.refresh("c1", 0, &ids(&["x"])).result, RefreshResult::Released);
    assert_eq!(s.held("x"), 0);
    // Refresh first: it re-registers, the release drops it.
    assert_eq!(s.refresh("c2", 0, &ids(&["x"])).result, RefreshResult::Reregistered);
    assert_eq!(s.held("x"), 1);
    s.release(&["c2"]);
    assert_eq!(s.held("x"), 0);
    assert_eq!(s.refresh("c2", 0, &ids(&["x"])).result, RefreshResult::Released);
    assert_eq!(s.calls(), 0);
}

/// A release of a key whose set an admit dropped upgrades the drop fence to a
/// release fence: a later admit of the key is refused, not an ordinary admit.
#[tokio::test(start_paused = true)]
async fn a_release_behind_a_drop_fence_refuses_a_later_admit() {
    let s = store();
    witnesses(&s, &["x", "z"]);
    assert_eq!(s.admit("c1", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(refused_on(s.admit("c1", n(), &entries(&[("z", 1)]), true)), "z");
    s.release(&["c1"]);
    assert_eq!(s.stats().fences, 1, "one fence for the key");
    assert_eq!(
        s.admit("c1", n(), &entries(&[("x", 10)]), false),
        AdmitResult::Released,
        "the release fence refuses the admit"
    );
    assert_eq!(s.refresh("c1", 0, &ids(&["x"])).result, RefreshResult::Released);
    assert_eq!([s.held("x"), s.held("z")], [1, 1], "the witnesses");
}

// ── change numbers: an older admit never overwrites a newer set ─────────

/// An admit numbered at or below the set the call holds changes nothing and
/// states the set held: a late admit and a repeated one alike.
#[tokio::test(start_paused = true)]
async fn an_admit_not_above_the_held_set_s_number_is_superseded() {
    let s = store();
    assert_eq!(s.admit("c1", 5, &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    let held = HeldSet { change: 5, entries: entries(&[("x", 10)]) };
    assert_eq!(
        s.admit("c1", 4, &entries(&[("y", 10)]), false),
        AdmitResult::Superseded { held: held.clone() },
        "a late admit"
    );
    assert_eq!(
        s.admit("c1", 5, &entries(&[("y", 10)]), false),
        AdmitResult::Superseded { held: held.clone() },
        "a second admit under the same number"
    );
    assert_eq!([s.held("x"), s.held("y")], [1, 0], "nothing moved");
    assert_eq!(s.admit("c1", 6, &entries(&[("y", 10)]), false), AdmitResult::Admitted);
    assert_eq!([s.held("x"), s.held("y")], [0, 1]);
}

/// A cap refusal keeping the set records its number: an admit sent before it
/// and landing after it is superseded, and the refusal states the set kept.
#[tokio::test(start_paused = true)]
async fn a_cap_refusal_orders_the_admits_sent_before_it() {
    let s = store();
    witnesses(&s, &["z"]);
    assert_eq!(s.admit("c1", 1, &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(
        s.admit("c1", 3, &entries(&[("x", 10), ("z", 1)]), false),
        AdmitResult::Rejected {
            limiter_id: "z".into(),
            held: HeldSet { change: 3, entries: entries(&[("x", 10)]) },
        },
        "the set kept, under the refusal's number"
    );
    assert!(
        matches!(s.admit("c1", 2, &entries(&[("y", 10)]), false), AdmitResult::Superseded { .. }),
        "the admit sent before the refusal lands after it"
    );
    assert_eq!([s.held("x"), s.held("y")], [1, 0]);
}

/// A drop keeps its number on the fence: an older admit landing after an
/// empty replacement or a refusal that dropped the set re-creates nothing.
#[tokio::test(start_paused = true)]
async fn a_drop_fence_orders_the_admits_sent_before_it() {
    let s = store();
    witnesses(&s, &["z"]);
    assert_eq!(s.admit("c1", 1, &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c1", 3, &[], false), AdmitResult::Admitted);
    assert_eq!(
        s.admit("c1", 2, &entries(&[("x", 10)]), false),
        AdmitResult::Superseded { held: HeldSet { change: 3, entries: vec![] } }
    );
    assert_eq!(s.admit("c2", 1, &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(
        s.admit("c2", 3, &entries(&[("z", 1)]), true),
        AdmitResult::Rejected {
            limiter_id: "z".into(),
            held: HeldSet { change: 3, entries: vec![] },
        },
        "the refusal dropped the set and says so"
    );
    assert!(matches!(
        s.admit("c2", 2, &entries(&[("x", 10)]), false),
        AdmitResult::Superseded { .. }
    ));
    assert_eq!(s.held("x"), 0);
}

/// A refusal of a key the store holds nothing for records its number for one
/// lease: an older admit landing after it is superseded, the answer states the
/// number, and the marker fences nothing (a refresh still re-registers). The
/// marker lapses with the lease.
#[tokio::test(start_paused = true)]
async fn a_refusal_of_a_key_holding_nothing_orders_the_admits_sent_before_it() {
    let s = store();
    witnesses(&s, &["z"]);
    assert_eq!(
        s.admit("c1", 3, &entries(&[("z", 1)]), false),
        AdmitResult::Rejected {
            limiter_id: "z".into(),
            held: HeldSet { change: 3, entries: vec![] }
        }
    );
    assert_eq!(
        s.admit("c1", 2, &entries(&[("x", 10)]), false),
        AdmitResult::Superseded { held: HeldSet { change: 3, entries: vec![] } },
        "the admit sent before the refusal lands after it"
    );
    assert_eq!(s.held("x"), 0);
    assert_eq!(s.stats().fences, 0, "a number marker is no fence");
    assert_eq!(s.refresh("c2", 0, &ids(&[])).result, RefreshResult::Released);
    s.refresh("c1", 3, &ids(&["x"]));
    assert_eq!(s.held("x"), 1, "the marker fences no refresh");
    s.release(&["c1"]);

    assert_eq!(
        s.admit("c3", 5, &entries(&[("z", 1)]), false),
        AdmitResult::Rejected {
            limiter_id: "z".into(),
            held: HeldSet { change: 5, entries: vec![] }
        }
    );
    advance(LEASE + Duration::from_secs(1)).await;
    assert_eq!(
        s.admit("c3", 4, &entries(&[("x", 10)]), false),
        AdmitResult::Admitted,
        "the marker lapsed with its lease"
    );
}

/// A drop fence taking over a key that held only a change marker carries its
/// number: the marker goes, and the gauge counts the fence alone.
#[tokio::test(start_paused = true)]
async fn a_drop_fence_taking_over_a_key_removes_its_change_marker() {
    let s = store();
    witnesses(&s, &["z"]);
    assert_eq!(refused_on(s.admit("c1", 3, &entries(&[("z", 1)]), false)), "z");
    assert_eq!((s.stats().change_markers, s.stats().fences), (1, 0), "a marker");
    assert_eq!(refused_on(s.admit("c1", 4, &entries(&[("z", 1)]), true)), "z");
    assert_eq!((s.stats().change_markers, s.stats().fences), (0, 1), "the drop fence alone");
    assert!(matches!(
        s.admit("c1", 4, &entries(&[("x", 10)]), false),
        AdmitResult::Superseded { .. }
    ));
}

/// Every refresh answer but a released call's states the set held for the
/// key under its number: the set extended, the set re-registered from what
/// the refresh carried, the drop fence's empty set.
#[tokio::test(start_paused = true)]
async fn every_refresh_answer_states_the_set_held() {
    let s = store();
    assert_eq!(s.admit("c1", 4, &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    let refreshed = s.refresh("c1", 3, &ids(&["x"]));
    assert_eq!(refreshed.result, RefreshResult::Extended);
    assert_eq!(
        refreshed.held,
        Some(HeldSet { change: 4, entries: entries(&[("x", 10), ("y", 10)]) }),
        "what the store holds, not what the refresh carried"
    );
    let refreshed = s.refresh("c2", 7, &ids(&["y"]));
    assert_eq!(refreshed.result, RefreshResult::Reregistered);
    assert_eq!(refreshed.held, Some(HeldSet { change: 7, entries: ids(&["y"]) }));
    assert_eq!(s.admit("c3", 2, &[], false), AdmitResult::Admitted);
    let refreshed = s.refresh("c3", 1, &ids(&["y"]));
    assert_eq!(refreshed.result, RefreshResult::Dropped);
    assert_eq!(refreshed.held, Some(HeldSet { change: 2, entries: vec![] }));
    s.release(&["c1"]);
    assert_eq!(s.refresh("c1", 4, &ids(&["x"])).held, None, "a released call states none");
    assert_eq!(s.held_set("c1"), None);
    assert_eq!(s.held_set("c2"), Some(HeldSet { change: 7, entries: ids(&["y"]) }));
}

// ── wire ──────────────────────────────────────────────────────────────

fn addr() -> std::net::SocketAddr {
    "10.0.0.1:8080".parse().unwrap()
}

async fn call(net: &SimulatedHttpNetwork, req: HttpRequest) -> HttpResponse {
    let h = tokio::spawn({
        let net = net.clone();
        async move { net.request(addr(), req).await }
    });
    advance(Duration::from_millis(5)).await;
    h.await.unwrap().unwrap()
}

/// A release of an unknown key answers 200 on the wire and creates nothing.
#[tokio::test(start_paused = true)]
async fn a_release_of_an_unknown_key_answers_200_and_creates_nothing() {
    let store = Arc::new(store());
    witnesses(&store, &["x"]);
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server).await.unwrap();
    let body = serde_json::to_vec(&ReleaseRequest { keys: vec!["c1".into()] }).unwrap();
    assert_eq!(call(&net, HttpRequest::post("/v1/release", body)).await.status, 200);
    assert_eq!(store.held("x"), 1, "the witness");
    assert_eq!(store.calls(), 1, "no set created");
    assert_eq!(store.stats().fences, 1, "the key is fenced");
}

/// One release request names several calls: every known one is freed, an
/// unknown one creates nothing, and the answer is 200.
#[tokio::test(start_paused = true)]
async fn one_release_request_frees_every_call_it_names() {
    let store = Arc::new(store());
    witnesses(&store, &["x", "y"]);
    assert_eq!(
        store.admit("c1", n(), &entries(&[("x", 10), ("y", 10)]), false),
        AdmitResult::Admitted
    );
    assert_eq!(store.admit("c2", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server).await.unwrap();
    let keys = vec!["c1".into(), "unknown".into(), "c2".into()];
    let body = serde_json::to_vec(&ReleaseRequest { keys }).unwrap();
    assert_eq!(call(&net, HttpRequest::post("/v1/release", body)).await.status, 200);
    assert_eq!([store.held("x"), store.held("y")], [1, 1], "the witnesses");
    assert_eq!(store.calls(), 2, "the witnesses");
    assert_eq!(store.stats().fences, 3, "every named key is fenced");
}

/// The HTTP surface carries the call key: an admit names its call and
/// `release_on_refusal`; a release names the call and answers 200 whether
/// or not the call is known; a refresh says whether the call was known; a
/// released call's admit answers its own reason.
#[tokio::test(start_paused = true)]
async fn the_wire_carries_the_call_key() {
    let store = Arc::new(store());
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server).await.unwrap();

    let admit = |key: &str, entries: Vec<AdmitEntry>, release_on_refusal: bool| {
        let change = n();
        serde_json::to_vec(&AdmitRequest {
            key: key.into(),
            change,
            held: HeldSet::default(),
            entries,
            release_on_refusal,
        })
        .unwrap()
    };
    let resp =
        call(&net, HttpRequest::post("/v1/admit", admit("c1", entries(&[("x", 1)]), false))).await;
    assert_eq!(resp.status, 200);
    let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body.outcome, AdmitAnswer::Admitted);

    let resp =
        call(&net, HttpRequest::post("/v1/admit", admit("c2", entries(&[("x", 1)]), false))).await;
    let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body.outcome, AdmitAnswer::Rejected, "a cap refusal, not a released-call one");
    assert_eq!(body.rejected_id.as_deref(), Some("x"));

    let refresh = |key: &str| {
        let calls = vec![RefreshCall { key: key.into(), change: 0, entries: ids(&["x"]) }];
        serde_json::to_vec(&RefreshRequest { calls }).unwrap()
    };
    let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c1"))).await;
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(outcomes(&body), [RefreshAnswer::Extended]);

    let release =
        |key: &str| serde_json::to_vec(&ReleaseRequest { keys: vec![key.into()] }).unwrap();
    let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c3"))).await;
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(outcomes(&body), [RefreshAnswer::Reregistered], "an unknown call is re-registered");
    assert_eq!(store.held("x"), 2);
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release("c3"))).await.status, 200);
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release("c1"))).await.status, 200);
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release("c1"))).await.status, 200);
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release("nope"))).await.status, 200);
    assert_eq!(store.held("x"), 0);

    let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c1"))).await;
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(outcomes(&body), [RefreshAnswer::Released], "a released call is refused by refresh");

    let resp =
        call(&net, HttpRequest::post("/v1/admit", admit("c1", entries(&[("x", 1)]), false))).await;
    let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(
        body.outcome,
        AdmitAnswer::Released,
        "the release fence refuses with its own reason"
    );
    assert!(body.rejected_id.is_none() && body.held.is_none());
    assert_eq!(store.held("x"), 0);

    // An empty replacement drops the set: a refresh then answers `dropped`.
    let resp = call(&net, HttpRequest::post("/v1/admit", admit("c4", vec![], true))).await;
    let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body.outcome, AdmitAnswer::Admitted);
    let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c4"))).await;
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(
        outcomes(&body),
        [RefreshAnswer::Dropped],
        "a dropped set is refused with its reason"
    );
    assert_eq!(store.held("x"), 0);
}

/// Every admit and refresh answer states the store's lease, whatever its
/// outcome.
#[tokio::test(start_paused = true)]
async fn every_admit_and_refresh_answer_states_the_lease() {
    let store = Arc::new(store());
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server).await.unwrap();
    let lease_ms = LEASE.as_millis() as u64;

    let admit = |key: &str, cap: i64| {
        let entries = entries(&[("x", cap), ("y", 10)]);
        let request = AdmitRequest {
            key: key.into(),
            change: n(),
            held: HeldSet::default(),
            entries,
            release_on_refusal: false,
        };
        serde_json::to_vec(&request).unwrap()
    };
    let admitted = call(&net, HttpRequest::post("/v1/admit", admit("c1", 1))).await;
    let refused = call(&net, HttpRequest::post("/v1/admit", admit("c2", 1))).await;
    let release = serde_json::to_vec(&ReleaseRequest { keys: vec!["c1".into()] }).unwrap();
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release)).await.status, 200);
    let fenced = call(&net, HttpRequest::post("/v1/admit", admit("c1", 10))).await;
    for (why, resp) in [("admitted", admitted), ("cap refusal", refused), ("fence", fenced)] {
        let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body.lease_ms, lease_ms, "{why}: {body:?}");
    }

    let calls = ["c1", "c3"]
        .map(|key| RefreshCall { key: key.into(), change: 0, entries: ids(&["x", "y"]) })
        .to_vec();
    let body = serde_json::to_vec(&RefreshRequest { calls }).unwrap();
    let resp = call(&net, HttpRequest::post("/v1/refresh", body)).await;
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(outcomes(&body), [RefreshAnswer::Released, RefreshAnswer::Reregistered]);
    assert_eq!(body.lease_ms, lease_ms);

    let release = serde_json::to_vec(&ReleaseRequest { keys: vec!["c3".into()] }).unwrap();
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release)).await.status, 200);
    assert_eq!([store.held("x"), store.held("y")], [0, 0]);
}

/// One refresh request names many calls and answers each one as its own
/// refresh would, in the order named: a known call is extended, a lapsed one
/// re-registered, a released or dropped one refused with its fence, a call
/// named twice answered twice. The limiter counts the request once and every
/// call it names.
#[tokio::test(start_paused = true)]
async fn one_refresh_request_answers_every_call_it_names_in_order() {
    let store = Arc::new(store());
    let metrics = LimiterMetrics::new();
    let server = Arc::new(LimiterServer::new(store.clone(), metrics.clone()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server).await.unwrap();
    assert_eq!(
        store.admit("known", n(), &entries(&[("x", 10), ("y", 10)]), false),
        AdmitResult::Admitted
    );
    assert_eq!(store.admit("released", n(), &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    store.release(&["released"]);
    assert_eq!(store.admit("dropped", n(), &[], false), AdmitResult::Admitted);

    let named = [
        ("known", vec!["x", "y"]),
        ("lapsed", vec!["x", "x"]),
        ("released", vec!["x"]),
        ("dropped", vec!["y"]),
        ("known", vec!["x", "y"]),
    ];
    let calls = named
        .iter()
        .map(|(key, named)| RefreshCall { key: (*key).into(), change: 0, entries: ids(named) })
        .collect();
    let body = serde_json::to_vec(&RefreshRequest { calls }).unwrap();
    let resp = call(&net, HttpRequest::post("/v1/refresh", body)).await;
    assert_eq!(resp.status, 200);
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(
        outcomes(&body),
        [
            RefreshAnswer::Extended,
            RefreshAnswer::Reregistered,
            RefreshAnswer::Released,
            RefreshAnswer::Dropped,
            RefreshAnswer::Extended,
        ]
    );
    assert_eq!([store.held("x"), store.held("y")], [3, 1], "known [x, y] and lapsed [x, x]");
    assert_eq!(store.stats().reregistered_calls, 1);
    let text = metrics.prometheus_text(store.stats());
    assert!(text.contains("limiter_refresh_requests_total 1\n"), "{text}");
    for (outcome, calls) in [("extended", 2), ("reregistered", 1), ("released", 1), ("dropped", 1)]
    {
        let line = format!("limiter_refresh_calls_total{{outcome=\"{outcome}\"}} {calls}\n");
        assert!(text.contains(&line), "{line} in {text}");
    }

    let x = ids(&["x"]);
    let direct = store.refresh_all([("known", 0, &x[..]), ("gone", 0, &[][..])]);
    let direct: Vec<_> = direct.into_iter().map(|r| r.result).collect();
    assert_eq!(direct, [RefreshResult::Extended, RefreshResult::Released]);
}
