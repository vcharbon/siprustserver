//! An admit carries the call's held set (ADR-0040 decision 2).
//!
//! A store that holds no set for the key and has not fenced it (it restarted
//! empty, or the set lapsed) re-registers the carried set with no cap check,
//! as a refresh carrying it would, and then checks the change net of it: an
//! id the call keeps is never checked against its cap, only the ids it adds.
//! A key fenced by a release or by a drop re-registers nothing, and a
//! superseded admit changes nothing.

use std::sync::Arc;

use call_limiter::wire::{AdmitAnswer, AdmitEntry, AdmitRequest, AdmitResponse, HeldSet};
use call_limiter::{
    AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer, RefreshResult,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;

fn store() -> CallStore {
    CallStore::new(LimiterConfig { lease_sec: 10 }, Clock::test_at(0))
}

fn entries(ids: &[(&str, i64)]) -> Vec<AdmitEntry> {
    ids.iter().map(|(id, limit)| AdmitEntry { id: (*id).into(), limit: *limit }).collect()
}

fn held(change: u64, ids: &[(&str, i64)]) -> HeldSet {
    HeldSet { change, entries: entries(ids) }
}

/// A store restarted empty with `x` at its cap of 2 again: one call admitted
/// since the restart, one re-registered by its refresh.
fn restarted_with_x_at_its_cap() -> CallStore {
    let s = store();
    assert_eq!(s.admit("since", 1, &entries(&[("x", 2)]), false), AdmitResult::Admitted);
    assert_eq!(s.refresh("peer", 7, &entries(&[("x", 2)])).result, RefreshResult::Reregistered);
    assert_eq!(s.held("x"), 2, "x at its cap");
    s
}

/// The call held `[x]` before the restart and keeps it while it adds `y`:
/// the admit re-registers `[x]` and checks only `y`, so it is admitted.
#[tokio::test(start_paused = true)]
async fn an_admit_re_registers_the_held_set_and_checks_only_the_ids_it_adds() {
    let s = restarted_with_x_at_its_cap();
    let r =
        s.admit_carrying("c1", 4, &held(3, &[("x", 2)]), &entries(&[("x", 2), ("y", 10)]), false);
    assert_eq!(r, AdmitResult::Admitted, "x is kept, not added");
    assert_eq!([s.held("x"), s.held("y")], [3, 1], "re-registration knows no cap");
    assert_eq!(s.held_set("c1"), Some(held(4, &[("x", 2), ("y", 10)])));
    assert_eq!(s.stats().admit_reregistered_calls, 1);
    s.release(&["c1"]);
    assert_eq!([s.held("x"), s.held("y")], [2, 0], "one release frees the call's whole set");
}

/// The change adds `z`, at its cap: the refusal keeps the re-registered set
/// and states it under the refusal's number, so the call stays counted.
#[tokio::test(start_paused = true)]
async fn a_refused_change_keeps_the_re_registered_set() {
    let s = restarted_with_x_at_its_cap();
    assert_eq!(s.admit("zed", 1, &entries(&[("z", 1)]), false), AdmitResult::Admitted);
    let r =
        s.admit_carrying("c1", 4, &held(3, &[("x", 2)]), &entries(&[("x", 2), ("z", 1)]), false);
    assert_eq!(
        r,
        AdmitResult::Rejected { limiter_id: "z".into(), held: held(4, &[("x", 2)]) },
        "refused on the id it adds, stating the set it holds"
    );
    assert_eq!([s.held("x"), s.held("z")], [3, 1]);
    assert_eq!(s.refresh("c1", 4, &entries(&[("x", 2)])).result, RefreshResult::Extended);
}

/// The same refusal with `release_on_refusal`: the re-registered set is
/// dropped in the same step, behind a drop fence.
#[tokio::test(start_paused = true)]
async fn a_refusal_with_release_on_refusal_drops_the_re_registered_set() {
    let s = restarted_with_x_at_its_cap();
    assert_eq!(s.admit("zed", 1, &entries(&[("z", 1)]), false), AdmitResult::Admitted);
    let r = s.admit_carrying("c1", 4, &held(3, &[("x", 2)]), &entries(&[("x", 2), ("z", 1)]), true);
    assert_eq!(r, AdmitResult::Rejected { limiter_id: "z".into(), held: held(4, &[]) });
    assert_eq!([s.held("x"), s.held("z")], [2, 1], "nothing of the call is left");
    assert_eq!(s.refresh("c1", 4, &entries(&[("x", 2)])).result, RefreshResult::Dropped);
}

/// A key the call's own admit dropped keeps its drop fence: an admit that
/// lost that answer and still carries the old set re-registers nothing, and
/// every id it names is checked as added.
#[tokio::test(start_paused = true)]
async fn a_drop_fence_re_registers_nothing() {
    let s = store();
    assert_eq!(s.admit("c1", 1, &entries(&[("x", 10)]), false), AdmitResult::Admitted);
    assert_eq!(s.admit("c1", 2, &[], false), AdmitResult::Admitted, "the call dropped its set");
    assert_eq!(s.admit("other", 1, &entries(&[("x", 1)]), false), AdmitResult::Admitted);
    let r = s.admit_carrying("c1", 3, &held(1, &[("x", 10)]), &entries(&[("x", 1)]), false);
    assert_eq!(r, AdmitResult::Rejected { limiter_id: "x".into(), held: held(3, &[]) });
    assert_eq!(s.held("x"), 1);
    assert_eq!(s.stats().admit_reregistered_calls, 0);
}

/// A released key re-registers nothing: the admit is refused by the fence.
#[tokio::test(start_paused = true)]
async fn a_release_fence_re_registers_nothing() {
    let s = store();
    s.release(&["c1"]);
    let r = s.admit_carrying("c1", 3, &held(1, &[("x", 10)]), &entries(&[("x", 10)]), false);
    assert_eq!(r, AdmitResult::Released);
    assert_eq!((s.held("x"), s.calls()), (0, 0));
}

/// A key the store knows by a change marker only (it answered an admit of the
/// key while holding no set) re-registers the carried set as a refresh would:
/// a marker orders admits and fences nothing.
#[tokio::test(start_paused = true)]
async fn a_change_marker_re_registers_like_a_refresh() {
    let s = restarted_with_x_at_its_cap();
    assert_eq!(s.admit("zed", 1, &entries(&[("z", 1)]), false), AdmitResult::Admitted);
    let refused = s.admit("c1", 5, &entries(&[("z", 1)]), false);
    assert_eq!(refused, AdmitResult::Rejected { limiter_id: "z".into(), held: held(5, &[]) });
    assert_eq!(s.stats().change_markers, 1);
    let r =
        s.admit_carrying("c1", 6, &held(3, &[("x", 2)]), &entries(&[("x", 2), ("y", 10)]), false);
    assert_eq!(r, AdmitResult::Admitted);
    assert_eq!([s.held("x"), s.held("y")], [3, 1]);
    assert_eq!(s.stats().change_markers, 0);
}

/// An admit not above the number the store knows for the key is superseded
/// and re-registers nothing: the call's refresh re-registers its set.
#[tokio::test(start_paused = true)]
async fn a_superseded_admit_re_registers_nothing() {
    let s = store();
    assert_eq!(s.admit("zed", 1, &entries(&[("z", 1)]), false), AdmitResult::Admitted);
    assert!(matches!(s.admit("c1", 5, &entries(&[("z", 1)]), false), AdmitResult::Rejected { .. }));
    let r = s.admit_carrying("c1", 4, &held(3, &[("x", 10)]), &entries(&[("x", 10)]), false);
    assert_eq!(r, AdmitResult::Superseded { held: held(5, &[]) });
    assert_eq!(s.held("x"), 0);
}

/// A key the store holds a set for keeps it: the carried set is ignored, the
/// change is checked net of what the store holds.
#[tokio::test(start_paused = true)]
async fn a_known_set_is_not_replaced_by_the_carried_one() {
    let s = store();
    assert_eq!(s.admit("c1", 5, &entries(&[("y", 10)]), false), AdmitResult::Admitted);
    let r = s.admit_carrying("c1", 6, &held(3, &[("x", 10)]), &entries(&[("y", 10)]), false);
    assert_eq!(r, AdmitResult::Admitted);
    assert_eq!([s.held("x"), s.held("y")], [0, 1]);
    assert_eq!(s.stats().admit_reregistered_calls, 0);
}

/// The HTTP admit carries the held set: a restarted server re-registers it
/// before the check.
#[tokio::test(start_paused = true)]
async fn the_wire_admit_carries_the_held_set() {
    let store = Arc::new(restarted_with_x_at_its_cap());
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let addr: std::net::SocketAddr = "10.0.0.1:8080".parse().unwrap();
    let _h = net.serve(addr, server).await.unwrap();
    let body = serde_json::to_vec(&AdmitRequest {
        key: "c1".into(),
        change: 4,
        held: held(3, &[("x", 2)]),
        entries: entries(&[("x", 2), ("y", 10)]),
        release_on_refusal: false,
    })
    .unwrap();
    let resp: HttpResponse = {
        let net = net.clone();
        let h = tokio::spawn(async move {
            net.request(addr, HttpRequest::post("/v1/admit", body)).await.unwrap()
        });
        tokio::time::advance(std::time::Duration::from_millis(50)).await;
        h.await.unwrap()
    };
    let answer: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(answer.outcome, AdmitAnswer::Admitted);
    assert_eq!(answer.held, Some(held(4, &[("x", 2), ("y", 10)])));
    assert_eq!([store.held("x"), store.held("y")], [3, 1]);
}
