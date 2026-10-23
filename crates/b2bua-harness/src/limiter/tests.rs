use b2bua::limiter::LimiterHeld;

use super::doubles::{admit_all, answers_released, fail_open, refuse_all};
use super::*;

/// One admit through `sut`'s ledger over a limiter that fails open.
async fn one_fail_open(sut: &SutLimiter) {
    let limiter = CountingLimiter { inner: fail_open(), ledger: sut.ledger() };
    limiter.admit("c1", 1, &LimiterHeld::default(), &[entry("x"), entry("y")], false).await;
    assert_eq!(sut.count().failed_open, 1);
}

#[tokio::test]
#[should_panic(expected = "the default limiter failed open on 1 admit(s)")]
async fn a_fail_open_on_the_default_limiter_fails_the_check() {
    let (_client, sut) = SutLimiter::serve_default().await;
    one_fail_open(&sut).await;
    sut.assert_drained(&LimiterLeak::NONE);
}

#[tokio::test]
async fn a_fail_open_on_a_test_s_own_limiter_passes_the_check() {
    let sut = SutLimiter::own(None);
    one_fail_open(&sut).await;
    sut.assert_drained(&LimiterLeak::NONE);
}

fn entry(id: &str) -> LimiterEntry {
    LimiterEntry { id: id.into(), limit: 10 }
}

fn counting(inner: Arc<dyn CallLimiter>) -> (CountingLimiter, Arc<HoldLedger>) {
    let ledger = Arc::new(HoldLedger::default());
    (CountingLimiter { inner, ledger: ledger.clone() }, ledger)
}

#[tokio::test]
async fn a_probe_reads_the_count_after_its_sut_side_moves_on() {
    let (_client, sut) = SutLimiter::serve_default().await;
    let probe = sut.probe();
    let limiter = CountingLimiter { inner: admit_all(), ledger: sut.ledger() };
    limiter.admit("c1", 2, &LimiterHeld::default(), &[entry("x"), entry("y")], false).await;
    limiter.admit("c2", 3, &LimiterHeld::default(), &[entry("x")], false).await;
    limiter.release(&["c1".to_string()]).await;
    assert_eq!(probe.count(), sut.count());
    assert_eq!((probe.count().admitted, probe.count().released), (3, 2));
}

#[tokio::test]
async fn every_granted_set_released_once_matches_no_leak() {
    let (limiter, ledger) = counting(admit_all());
    limiter
        .admit("c1", 4, &LimiterHeld::default(), &[entry("x"), entry("x"), entry("y")], false)
        .await;
    let held = LimiterHeld { change: 4, entries: vec![entry("x")] };
    limiter.refresh(&[RefreshCall { key: "c1".into(), held }]).await;
    limiter.release(&["c1".to_string()]).await;
    ledger.count(None).assert_matches(&LimiterLeak::NONE);
}

#[tokio::test]
async fn a_replaced_set_counts_as_released() {
    let (limiter, ledger) = counting(admit_all());
    limiter.admit("c1", 5, &LimiterHeld::default(), &[entry("x"), entry("y")], false).await;
    limiter.admit("c1", 6, &LimiterHeld::default(), &[entry("y"), entry("z")], true).await;
    assert_eq!((ledger.count(None).admitted, ledger.count(None).released), (4, 2));
    limiter.release(&["c1".to_string()]).await;
    ledger.count(None).assert_matches(&LimiterLeak::NONE);
}

#[tokio::test]
async fn a_refusal_that_drops_the_set_counts_it_as_released() {
    let (limiter, ledger) = counting(admit_all());
    limiter.admit("c1", 7, &LimiterHeld::default(), &[entry("x")], false).await;
    let (refusing, _) = counting(refuse_all(answers_released()));
    let refusing = CountingLimiter { inner: refusing.inner, ledger: ledger.clone() };
    refusing.admit("c1", 8, &LimiterHeld::default(), &[entry("y")], false).await;
    assert_eq!(ledger.count(None).unreleased(), 1, "a refusal keeping the set");
    refusing.admit("c1", 9, &LimiterHeld::default(), &[entry("y")], true).await;
    ledger.count(None).assert_matches(&LimiterLeak::NONE);
}

#[tokio::test]
#[should_panic(expected = "1 hold(s) granted and never released")]
async fn a_granted_set_never_released_is_a_leak() {
    let (limiter, ledger) = counting(admit_all());
    limiter.admit("c1", 10, &LimiterHeld::default(), &[entry("x")], false).await;
    limiter.admit("c2", 11, &LimiterHeld::default(), &[entry("y")], false).await;
    limiter.release(&["c1".to_string()]).await;
    ledger.count(None).assert_matches(&LimiterLeak::NONE);
}

#[tokio::test]
async fn a_second_release_of_one_call_counts_nothing() {
    let (limiter, ledger) = counting(admit_all());
    limiter.admit("c1", 12, &LimiterHeld::default(), &[entry("x")], false).await;
    limiter.release(&["c1".to_string()]).await;
    limiter.release(&["c1".to_string()]).await;
    limiter.release(&["never-admitted".to_string()]).await;
    assert_eq!(ledger.count(None).released, 1);
    ledger.count(None).assert_matches(&LimiterLeak::NONE);
}

#[tokio::test]
async fn a_declared_unreleased_hold_matches() {
    let (limiter, ledger) = counting(admit_all());
    limiter.admit("c1", 13, &LimiterHeld::default(), &[entry("x"), entry("y")], false).await;
    ledger.count(None).assert_matches(&LimiterLeak {
        unreleased: 2,
        stored: 0,
        queued: Vec::new(),
    });
}

#[test]
#[should_panic(expected = "the store still counts 1 hold(s)")]
fn a_hold_the_store_still_counts_is_a_leak() {
    let count = LimiterCount { admitted: 1, released: 1, failed_open: 0, stored: Some(1) };
    count.assert_matches(&LimiterLeak::NONE);
}

#[tokio::test]
async fn only_routes_gain_the_default_entry() {
    use b2bua::decision::test_adapter::route_to;
    use b2bua::decision::RejectDecision;
    let mut own = route_to("127.0.0.1", 5070);
    own.call_limiter = vec![LimiterEntry { id: "x".into(), limit: 3 }];
    let CallTreatment::Route(route) = treatment_with_default_entry(CallTreatment::Route(own))
    else {
        panic!("a route stays a route");
    };
    let ids: Vec<&str> = route.call_limiter.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["x", DEFAULT_LIMITER_ID], "the route's own entries come first");
    let reject = CallTreatment::Reject(RejectDecision {
        reject_code: 486,
        reject_reason: None,
        update_headers: None,
        service_ext: Default::default(),
        label: None,
    });
    assert!(matches!(treatment_with_default_entry(reject), CallTreatment::Reject(_)));
}
