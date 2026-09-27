//! Admission holds keyed by the call.
//!
//! The store keeps, per `call_ref`, the multiset of ids the call holds and a
//! lease. `admit(call_ref, entries, release_on_refusal)` replaces the call's
//! set atomically, checked net of the call's own set; `release(call_ref)` is
//! idempotent and tombstones the call for one lease; `refresh(call_ref)`
//! extends the lease; a set whose lease lapses is dropped and counted.
//!
//! Every scenario carries a **witness** hold on each id, admitted for a call
//! of its own, so a surplus release reads below the witness instead of
//! vanishing under the floor at 0.

use std::sync::Arc;
use std::time::Duration;

use call_limiter::wire::{
    AdmitEntry, AdmitRequest, AdmitResponse, RefreshRequest, RefreshResponse, ReleaseRequest,
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
        let r = s.admit(&format!("witness-{id}"), &[entry(id, 100)], false);
        assert_eq!(r, AdmitResult::Admitted, "witness on {id}");
    }
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
    assert_eq!(s.admit("c1", &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    // y: the witness + the call = 2 = the cap.
    assert_eq!(s.admit("c1", &entries(&[("y", 2), ("z", 10)]), false), AdmitResult::Admitted);
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
    assert_eq!(s.admit("c1", &entries(&[("x", 10), ("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 3);
    assert_eq!(s.admit("c1", &entries(&[("x", 2)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 2, "the witness and one hold of the call");
}

/// An id the call adds is checked against the cap: `[x]` → `[x, y]` with
/// `y` at its cap is refused on `y`, and nothing moves.
#[tokio::test(start_paused = true)]
async fn a_replacement_adding_an_id_at_its_cap_is_refused() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(
        s.admit("c1", &entries(&[("x", 10), ("y", 1)]), false),
        AdmitResult::Rejected { limiter_id: "y".into() }
    );
    assert_eq!([s.held("x"), s.held("y")], [2, 1], "the old set stays, y untouched");
}

/// The same id twice in one list takes two slots.
#[tokio::test(start_paused = true)]
async fn the_same_id_twice_on_one_list_takes_two_slots() {
    let s = store();
    witnesses(&s, &["x"]);
    // Room for one more only: the second slot is refused, nothing counted.
    assert_eq!(
        s.admit("c1", &entries(&[("x", 2), ("x", 2)]), false),
        AdmitResult::Rejected { limiter_id: "x".into() }
    );
    assert_eq!(s.held("x"), 1);
    assert_eq!(s.admit("c1", &entries(&[("x", 3), ("x", 3)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 3);
    s.release("c1");
    assert_eq!(s.held("x"), 1, "both slots released at once");
}

/// A refusal on the second entry increments nothing, not even the first,
/// and keeps the call's old set when `release_on_refusal` is off.
#[tokio::test(start_paused = true)]
async fn a_refusal_on_the_second_entry_keeps_the_old_set_and_counts_nothing() {
    let s = store();
    witnesses(&s, &["w", "x", "y"]);
    assert_eq!(s.admit("c1", &entries(&[("w", 10)]), false), AdmitResult::Admitted);
    assert_eq!(
        s.admit("c1", &entries(&[("x", 10), ("y", 1)]), false),
        AdmitResult::Rejected { limiter_id: "y".into() }
    );
    assert_eq!([s.held("w"), s.held("x"), s.held("y")], [2, 1, 1]);
}

/// With `release_on_refusal`, a refused replacement drops the old set in
/// the same step: the call holds nothing, the witnesses are intact.
#[tokio::test(start_paused = true)]
async fn a_refused_replacement_with_release_on_refusal_drops_the_old_set() {
    let s = store();
    witnesses(&s, &["w", "x", "y"]);
    assert_eq!(s.admit("c1", &entries(&[("w", 10), ("w", 10)]), false), AdmitResult::Admitted);
    assert_eq!(
        s.admit("c1", &entries(&[("x", 10), ("y", 1)]), true),
        AdmitResult::Rejected { limiter_id: "y".into() }
    );
    assert_eq!([s.held("w"), s.held("x"), s.held("y")], [1, 1, 1], "only the witnesses");
    assert_eq!(s.calls(), 3, "the refused call holds no set");
    // A later admit of the same call is an ordinary admit (no tombstone:
    // the call was not released, its replacement was refused).
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 2);
}

/// An empty list replaces the call's set with nothing: the holds are freed,
/// the call is not tombstoned, and a later admit of the call is ordinary.
#[tokio::test(start_paused = true)]
async fn an_empty_replacement_frees_the_set_without_a_tombstone() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(s.admit("c1", &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c1", &[], true), AdmitResult::Admitted);
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "only the witnesses");
    assert_eq!(s.calls(), 2, "the call holds no set");
    assert_eq!(s.admit("c1", &entries(&[("y", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("y"), 2);
}

/// Each admit is checked against its own entries' caps: the store keeps no
/// cap per id.
#[tokio::test(start_paused = true)]
async fn each_admit_is_checked_against_its_own_entry_cap() {
    let s = store();
    witnesses(&s, &["x"]);
    assert_eq!(s.admit("c1", &entries(&[("x", 2)]), false), AdmitResult::Admitted);
    // c2 states a higher cap: admitted under it.
    assert_eq!(s.admit("c2", &entries(&[("x", 3)]), false), AdmitResult::Admitted);
    assert_eq!(
        s.admit("c3", &entries(&[("x", 3)]), false),
        AdmitResult::Rejected { limiter_id: "x".into() }
    );
}

/// An id the call keeps or reduces is never checked: `[x(10)]` → `[x(1)]`
/// with two witnesses on `x` is admitted although `x` is over the new cap.
#[tokio::test(start_paused = true)]
async fn a_kept_id_over_a_lower_cap_is_admitted() {
    let s = store();
    witnesses(&s, &["x"]);
    assert_eq!(s.admit("witness-x-2", &entries(&[("x", 100)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 3);
    assert_eq!(s.admit("c1", &entries(&[("x", 1)]), true), AdmitResult::Admitted);
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
        s.admit("c1", &entries(&[("x", 10), ("x", 10), ("y", 10)]), false),
        AdmitResult::Admitted
    );
    assert_eq!(s.stats().admission_max, 3, "x is held three times");
    s.release("c1");
    assert_eq!(s.stats().admission_max, 1, "the witnesses");
    s.release("witness-x");
    s.release("witness-y");
    assert_eq!(s.stats().admission_max, 0);
}

// ── release ───────────────────────────────────────────────────────────

/// A second release of one call is a no-op; a release of an unknown call
/// is a no-op: the witnesses are intact.
#[tokio::test(start_paused = true)]
async fn release_is_idempotent_and_unknown_calls_are_a_no_op() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(s.admit("c1", &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    s.release("c1");
    s.release("c1");
    s.release("never-admitted");
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "the witnesses");
    assert_eq!(s.stats().current_total, 2);
}

/// A released call is tombstoned for one lease: an admit is refused with a
/// distinct reason (not a cap refusal) and holds nothing; a refresh says
/// the call is unknown.
#[tokio::test(start_paused = true)]
async fn a_released_call_refuses_admit_and_refresh_for_one_lease() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    s.release("c1");
    assert_eq!(
        s.admit("c1", &entries(&[("y", 10)]), false),
        AdmitResult::Released,
        "a fold landing after the call ended holds nothing"
    );
    assert_eq!(s.held("y"), 1);
    assert_eq!(
        s.refresh("c1", &["x".into()]),
        RefreshResult::Released,
        "a refresh of a released call re-creates nothing"
    );
    assert_eq!(s.held("x"), 1);
    s.release("c1");
    assert_eq!([s.held("x"), s.held("y")], [1, 1], "a second release is a no-op");
}

/// A tombstone expires after one lease: the same `call_ref` is then an
/// ordinary new call.
#[tokio::test(start_paused = true)]
async fn a_tombstone_expires_after_one_lease() {
    let s = store();
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    s.release("c1");
    advance(LEASE - Duration::from_secs(1)).await;
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Released);
    advance(Duration::from_secs(2)).await;
    s.sweep_now();
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.held("x"), 1);
    s.release("c1");
}

// ── lease ─────────────────────────────────────────────────────────────

/// A set whose lease lapses is dropped by the sweep and counted: one
/// expired call, its holds.
#[tokio::test(start_paused = true)]
async fn lease_expiry_drops_the_set_and_counts_it() {
    let s = store();
    witnesses(&s, &["x", "y"]);
    assert_eq!(
        s.admit("c1", &entries(&[("x", 10), ("x", 10), ("y", 10)]), false),
        AdmitResult::Admitted
    );
    // Keep the witnesses alive across the lease.
    advance(LEASE / 2).await;
    assert_eq!(s.refresh("witness-x", &["x".into()]), RefreshResult::Extended);
    assert_eq!(s.refresh("witness-y", &["y".into()]), RefreshResult::Extended);
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
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c2", &entries(&[("y", 10)]), false), AdmitResult::Admitted);
    advance(LEASE - Duration::from_secs(1)).await;
    assert_eq!(s.refresh("c1", &["x".into()]), RefreshResult::Extended);
    assert_eq!(s.admit("c2", &entries(&[("y", 10), ("z", 10)]), false), AdmitResult::Admitted);
    advance(Duration::from_secs(2)).await;
    assert_eq!(s.sweep_now(), 0, "both leases were extended");
    assert_eq!([s.held("x"), s.held("y"), s.held("z")], [1, 1, 1]);
    advance(LEASE).await;
    assert_eq!(s.sweep_now(), 2);
    assert_eq!(s.stats().current_total, 0);
}

/// A refresh of a call the store does not know and has not tombstoned
/// re-creates its set from the ids it carries, with no cap check: the call
/// exists and was admitted.
#[tokio::test(start_paused = true)]
async fn a_refresh_of_an_unknown_call_re_registers_its_set_without_a_cap_check() {
    let s = store();
    witnesses(&s, &["x"]);
    assert_eq!(s.admit("c1", &entries(&[("x", 10), ("y", 10)]), false), AdmitResult::Admitted);
    advance(LEASE + Duration::from_secs(1)).await;
    assert_eq!(s.sweep_now(), 2, "the witness and the call lapsed");
    assert_eq!([s.held("x"), s.held("y")], [0, 0]);
    // x at cap 1 would refuse an admit; the refresh re-registers regardless.
    assert_eq!(s.admit("filler", &entries(&[("x", 1)]), false), AdmitResult::Admitted);
    assert_eq!(s.refresh("c1", &["x".into(), "x".into(), "y".into()]), RefreshResult::Reregistered);
    assert_eq!([s.held("x"), s.held("y")], [3, 1], "the set the refresh carries, cap or not");
    assert_eq!(s.stats().reregistered_calls, 1);
    assert_eq!(s.refresh("c1", &["x".into()]), RefreshResult::Extended, "known again");
    s.release("c1");
    assert_eq!([s.held("x"), s.held("y")], [1, 0], "one release frees the re-registered set");
}

/// A re-registration racing the call's release: whichever lands first, the
/// release wins and the store holds nothing for the call.
#[tokio::test(start_paused = true)]
async fn a_re_registration_racing_the_release_loses() {
    let s = store();
    // Release first: the tombstone refuses the refresh.
    assert_eq!(s.admit("c1", &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    s.release("c1");
    assert_eq!(s.refresh("c1", &["x".into()]), RefreshResult::Released);
    assert_eq!(s.held("x"), 0);
    // Refresh first: it re-registers, the release drops it.
    assert_eq!(s.refresh("c2", &["x".into()]), RefreshResult::Reregistered);
    assert_eq!(s.held("x"), 1);
    s.release("c2");
    assert_eq!(s.held("x"), 0);
    assert_eq!(s.refresh("c2", &["x".into()]), RefreshResult::Released);
    assert_eq!(s.calls(), 0);
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

/// The HTTP surface carries the call key: an admit names its call and
/// `release_on_refusal`; a release names the call and answers 200 whether
/// or not the call is known; a refresh says whether the call was known; a
/// tombstoned admit answers its own reason.
#[tokio::test(start_paused = true)]
async fn the_wire_carries_the_call_key() {
    let store = Arc::new(store());
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server).await.unwrap();

    let admit = |call_ref: &str, entries: Vec<AdmitEntry>, release_on_refusal: bool| {
        serde_json::to_vec(&AdmitRequest { call_ref: call_ref.into(), entries, release_on_refusal })
            .unwrap()
    };
    let resp =
        call(&net, HttpRequest::post("/v1/admit", admit("c1", entries(&[("x", 1)]), false))).await;
    assert_eq!(resp.status, 200);
    let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
    assert!(body.admitted);

    let resp =
        call(&net, HttpRequest::post("/v1/admit", admit("c2", entries(&[("x", 1)]), false))).await;
    let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
    assert!(!body.admitted);
    assert_eq!(body.rejected_id.as_deref(), Some("x"));
    assert!(!body.released, "a cap refusal is not a tombstone refusal");

    let refresh = |call_ref: &str| {
        serde_json::to_vec(&RefreshRequest { call_ref: call_ref.into(), ids: vec!["x".into()] })
            .unwrap()
    };
    let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c1"))).await;
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert!(body.known && !body.reregistered);

    let release =
        |call_ref: &str| serde_json::to_vec(&ReleaseRequest { call_ref: call_ref.into() }).unwrap();
    let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c3"))).await;
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert!(!body.known && body.reregistered, "an unknown call is re-registered");
    assert_eq!(store.held("x"), 2);
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release("c3"))).await.status, 200);
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release("c1"))).await.status, 200);
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release("c1"))).await.status, 200);
    assert_eq!(call(&net, HttpRequest::post("/v1/release", release("nope"))).await.status, 200);
    assert_eq!(store.held("x"), 0);

    let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c1"))).await;
    let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    assert!(!body.known && !body.reregistered, "a released call is refused by refresh");

    let resp =
        call(&net, HttpRequest::post("/v1/admit", admit("c1", entries(&[("x", 1)]), false))).await;
    let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
    assert!(!body.admitted);
    assert!(body.released, "the tombstone refuses with its own reason");
    assert!(body.rejected_id.is_none());
    assert_eq!(store.held("x"), 0);
}
