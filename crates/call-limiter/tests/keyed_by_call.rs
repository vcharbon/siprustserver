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
//!
//! The first module runs on today's `(id, window)` store and states its
//! defect; `contract` states the keyed store and is compiled out (`cfg(any())`)
//! until that store exists.

use call_limiter::wire::{AdmitEntry, Hold};
use call_limiter::{AdmitResult, LimiterConfig, WindowStore};
use sip_clock::Clock;

fn entry(id: &str, limit: i64) -> AdmitEntry {
    AdmitEntry { id: id.into(), limit }
}

/// A release is matched by `(id, window)` alone: a second release of one
/// call's hold takes the witness's count on the same key.
#[tokio::test(start_paused = true)]
async fn a_second_release_of_one_call_takes_a_neighbour_s_count() {
    let s = WindowStore::new(LimiterConfig::default(), Clock::test_at(0));
    let AdmitResult::Admitted { window } = s.admit(&[entry("x", 10)]) else {
        panic!("the witness is admitted")
    };
    assert!(matches!(s.admit(&[entry("x", 10)]), AdmitResult::Admitted { .. }), "the call");
    assert_eq!(s.held("x"), 2, "the witness and the call hold x");
    let hold = [Hold { id: "x".into(), window }];
    s.release(&hold);
    s.release(&hold);
    assert_eq!(s.held("x"), 1, "the second release of the call is a no-op: the witness stays");
}

/// The keyed store's contract. Compiled out until the store exists: the fix
/// phase removes the gate and settles the names.
#[cfg(any())]
mod contract {
    use std::sync::Arc;
    use std::time::Duration;

    use call_limiter::wire::{
        AdmitEntry, AdmitRequest, AdmitResponse, RefreshRequest, RefreshResponse, ReleaseRequest,
    };
    use call_limiter::{AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
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

    /// The cap of the most recent admit per id applies.
    #[tokio::test(start_paused = true)]
    async fn the_most_recent_limit_of_an_id_applies() {
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
        assert!(!s.refresh("c1"), "a refresh of a released call re-creates nothing");
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
        assert!(s.refresh("witness-x"));
        assert!(s.refresh("witness-y"));
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
        assert!(s.refresh("c1"));
        assert_eq!(s.admit("c2", &entries(&[("y", 10), ("z", 10)]), false), AdmitResult::Admitted);
        advance(Duration::from_secs(2)).await;
        assert_eq!(s.sweep_now(), 0, "both leases were extended");
        assert_eq!([s.held("x"), s.held("y"), s.held("z")], [1, 1, 1]);
        advance(LEASE).await;
        assert_eq!(s.sweep_now(), 2);
        assert_eq!(s.stats().current_total, 0);
    }

    /// A refresh of a call the store never admitted re-creates nothing.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_of_an_unknown_call_is_a_no_op() {
        let s = store();
        assert!(!s.refresh("never-admitted"));
        assert_eq!(s.calls(), 0);
        assert_eq!(s.stats().current_total, 0);
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
            serde_json::to_vec(&AdmitRequest {
                call_ref: call_ref.into(),
                entries,
                release_on_refusal,
            })
            .unwrap()
        };
        let resp =
            call(&net, HttpRequest::post("/v1/admit", admit("c1", entries(&[("x", 1)]), false)))
                .await;
        assert_eq!(resp.status, 200);
        let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
        assert!(body.admitted);

        let resp =
            call(&net, HttpRequest::post("/v1/admit", admit("c2", entries(&[("x", 1)]), false)))
                .await;
        let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
        assert!(!body.admitted);
        assert_eq!(body.rejected_id.as_deref(), Some("x"));
        assert!(!body.released, "a cap refusal is not a tombstone refusal");

        let refresh = |call_ref: &str| {
            serde_json::to_vec(&RefreshRequest { call_ref: call_ref.into() }).unwrap()
        };
        let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c1"))).await;
        let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
        assert!(body.known);

        let release = |call_ref: &str| {
            serde_json::to_vec(&ReleaseRequest { call_ref: call_ref.into() }).unwrap()
        };
        assert_eq!(call(&net, HttpRequest::post("/v1/release", release("c1"))).await.status, 200);
        assert_eq!(call(&net, HttpRequest::post("/v1/release", release("c1"))).await.status, 200);
        assert_eq!(call(&net, HttpRequest::post("/v1/release", release("nope"))).await.status, 200);
        assert_eq!(store.held("x"), 0);

        let resp = call(&net, HttpRequest::post("/v1/refresh", refresh("c1"))).await;
        let body: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
        assert!(!body.known, "a released call is unknown to refresh");

        let resp =
            call(&net, HttpRequest::post("/v1/admit", admit("c1", entries(&[("x", 1)]), false)))
                .await;
        let body: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
        assert!(!body.admitted);
        assert!(body.released, "the tombstone refuses with its own reason");
        assert!(body.rejected_id.is_none());
        assert_eq!(store.held("x"), 0);
    }
}
