use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};

use crate::limiter::lease::LimiterLease;
use crate::metrics::B2buaMetrics;
use http_net::{Fault, SimulatedHttpNetwork};
use sip_clock::Clock;

use super::*;
use crate::limiter::testkit::held_of;

const BUDGET: Duration = Duration::from_millis(150);

fn laddr() -> SocketAddr {
    "10.0.0.1:8080".parse().unwrap()
}

fn entries() -> Vec<LimiterEntry> {
    vec![LimiterEntry { id: "x".into(), limit: 1 }, LimiterEntry { id: "y".into(), limit: 1 }]
}

async fn served() -> (SimulatedHttpNetwork, Box<dyn http_net::HttpServerHandle>) {
    let net = SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store, LimiterMetrics::new()));
    let handle = net.serve(laddr(), server).await.unwrap();
    (net, handle)
}

#[tokio::test(start_paused = true)]
async fn the_health_answer_follows_the_limiter() {
    let (net, _server) = served().await;
    let client = HttpCallLimiter::new(Arc::new(net.clone()), laddr(), BUDGET);
    let health = client.health().expect("the HTTP client has a health answer");
    assert!(health.serving().await);
    net.apply_fault(Fault::Stall { dst: laddr() });
    assert!(!health.serving().await, "a stalled limiter misses the budget");
    net.apply_fault(Fault::Cut { dst: laddr() });
    assert!(!health.serving().await);
    net.apply_fault(Fault::Resume { dst: laddr() });
    assert!(health.serving().await);
}

/// Five admits of distinct calls sent at once through `limiter`.
async fn five_at_once(limiter: &Arc<dyn CallLimiter>) -> Vec<AdmitOutcome> {
    let admits: Vec<_> = (0..5)
        .map(|n| {
            let limiter = limiter.clone();
            let wide = vec![LimiterEntry { id: "x".into(), limit: 100 }];
            tokio::spawn(async move {
                limiter.admit(&format!("c{n}#k"), 1, &LimiterHeld::default(), &wide, false).await
            })
        })
        .collect();
    let mut outcomes = Vec::new();
    for admit in admits {
        outcomes.push(admit.await.unwrap());
    }
    outcomes
}

#[tokio::test(start_paused = true)]
async fn admits_during_a_first_lookup_inside_the_budget_wait_for_it_and_are_admitted() {
    let (net, _server) = served().await;
    let names = Arc::new(crate::resolved_target::tests::FakeResolver {
        delay: Some(Duration::from_millis(100)),
        ..Default::default()
    });
    names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
    let target = ResolvedTarget::name_with("limiter:8080", names.clone());
    let limiter: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::with_target(Arc::new(net), target, BUDGET));
    assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Admitted; 5]);
    assert_eq!(names.lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn admits_during_a_lookup_slower_than_the_budget_fail_and_the_lookup_still_lands() {
    let (net, _server) = served().await;
    let names = Arc::new(crate::resolved_target::tests::FakeResolver {
        delay: Some(Duration::from_millis(300)),
        ..Default::default()
    });
    names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
    let target = ResolvedTarget::name_with("limiter:8080", names.clone());
    let limiter: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::with_target(Arc::new(net), target, BUDGET));
    assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Unavailable; 5]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Admitted; 5]);
    assert_eq!(names.lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
}

fn resolver() -> Arc<crate::resolved_target::tests::FakeResolver> {
    Arc::new(Default::default())
}

#[tokio::test(start_paused = true)]
async fn a_name_that_does_not_resolve_fails_as_a_transport_error() {
    let (net, _server) = served().await;
    let target = ResolvedTarget::name_with("limiter:8080", resolver());
    let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
    assert_eq!(
        client.admit("c#k", 2, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Unavailable
    );
    let calls = [RefreshCall { key: "c#k".into(), held: held_of(&["x".into()]) }];
    assert_eq!(client.refresh(&calls).await, RefreshAnswer::Unavailable);
    assert_eq!(client.release(&["c#k".into()]).await, ReleaseAnswer::Unavailable);
    assert!(!client.health().unwrap().serving().await);
}

#[tokio::test(start_paused = true)]
async fn a_named_client_has_an_address_once_its_name_resolves_until_it_forgets_it() {
    let (net, _server) = served().await;
    let names = resolver();
    let target = ResolvedTarget::name_with("limiter:8080", names.clone());
    let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
    let health = client.health().unwrap();
    assert!(!health.has_address(), "never looked up");
    assert!(!client.lookup(BUDGET).await, "does not resolve");
    assert!(!health.has_address());
    names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
    assert!(client.lookup(BUDGET).await);
    assert!(health.has_address());
    health.forget_address();
    assert!(!health.has_address(), "forgotten");
    assert!(health.serving().await, "the probe looks it up again");
    assert!(health.has_address());
}

/// A limiter answering every refresh with one outcome whatever it names.
struct OneOutcome;

#[async_trait]
impl http_net::HttpService for OneOutcome {
    async fn handle(&self, _: HttpRequest) -> HttpResponse {
        let reply = wire::RefreshReply {
            outcome: wire::RefreshAnswer::Extended,
            held: Some(HeldSet { change: 1, entries: vec![] }),
        };
        let body = RefreshResponse { outcomes: vec![reply], lease_ms: 1_000 };
        HttpResponse::ok(serde_json::to_vec(&body).unwrap())
    }
}

#[tokio::test(start_paused = true)]
async fn one_refresh_request_answers_each_call_it_names() {
    let (net, _server) = served().await;
    let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
    assert_eq!(
        client.admit("known#k", 3, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Admitted
    );
    client.release(&["gone#k".into()]).await;
    let call = |key: &str| RefreshCall { key: key.into(), held: held_of(&["x".into()]) };
    let answer = client.refresh(&[call("known#k"), call("lapsed#k"), call("gone#k")]).await;
    let x = held_of(&["x".into()]);
    let RefreshAnswer::Answered(replies) = answer else { panic!("answered") };
    assert_eq!(
        replies,
        [
            RefreshReply {
                outcome: RefreshOutcome::Extended,
                held: Some(LimiterHeld { change: 3, entries: entries() }),
            },
            RefreshReply { outcome: RefreshOutcome::Reregistered, held: Some(x) },
            RefreshReply { outcome: RefreshOutcome::Released, held: None },
        ],
        "each reply states the set the limiter holds for its call"
    );
}

/// Every admit answer carries the set the limiter holds for the key: a
/// cap refusal the set kept, an admit numbered at or below the held set's
/// number the set held, untouched.
#[tokio::test(start_paused = true)]
async fn every_admit_answer_states_the_set_held() {
    let (net, _server) = served().await;
    let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
    let x = vec![LimiterEntry { id: "x".into(), limit: 1 }];
    let held = LimiterHeld { change: 2, entries: x.clone() };
    assert_eq!(
        client.admit("filler#k", 1, &LimiterHeld::default(), &entries()[1..], false).await,
        AdmitOutcome::Admitted
    );
    assert_eq!(
        client.admit("c#k", 2, &LimiterHeld::default(), &x, false).await,
        AdmitOutcome::Admitted
    );
    assert_eq!(
        client.admit("c#k", 1, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Superseded { held: held.clone() },
        "an older admit"
    );
    assert_eq!(
        client.admit("c#k", 3, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Rejected {
            limiter_id: "y".into(),
            held: LimiterHeld { change: 3, entries: x },
        },
        "the set kept, under the refusal's number"
    );
}

#[tokio::test(start_paused = true)]
async fn an_answer_naming_fewer_outcomes_than_calls_is_unavailable() {
    let net = SimulatedHttpNetwork::new();
    let _server = net.serve(laddr(), Arc::new(OneOutcome)).await.unwrap();
    let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
    let call = |key: &str| RefreshCall { key: key.into(), held: held_of(&["x".into()]) };
    assert_eq!(client.refresh(&[call("a#k"), call("b#k")]).await, RefreshAnswer::Unavailable);
}

#[tokio::test(start_paused = true)]
async fn every_admit_and_refresh_answer_hands_its_lease_to_every_registered_lease() {
    let (net, _server) = served().await;
    let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
    let (a, b) = (
        LimiterLease::starting_at(Duration::from_secs(20)),
        LimiterLease::starting_at(Duration::from_secs(30)),
    );
    client.report_to(LimiterReports::new(&a, B2buaMetrics::new()));
    client.report_to(LimiterReports::new(&b, B2buaMetrics::new()));
    assert_eq!(
        client.admit("c#k", 4, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Admitted
    );
    assert_eq!([a.current(), b.current()], [Duration::from_secs(120); 2], "the store's lease");
    a.learn(Duration::from_secs(5));
    let calls = [RefreshCall { key: "c#k".into(), held: held_of(&["x".into(), "y".into()]) }];
    assert_eq!(
        client.refresh(&calls).await,
        RefreshAnswer::Answered(vec![RefreshReply {
            outcome: RefreshOutcome::Extended,
            held: Some(LimiterHeld { change: 4, entries: entries() }),
        }])
    );
    assert_eq!(a.current(), Duration::from_secs(120), "a refresh answer states it too");
}

/// A limiter answering every admit with a body that states no lease.
struct NoLease;

#[async_trait]
impl http_net::HttpService for NoLease {
    async fn handle(&self, _: HttpRequest) -> HttpResponse {
        HttpResponse::ok(br#"{"admitted":true,"released":false}"#.to_vec())
    }
}

#[tokio::test(start_paused = true)]
async fn an_admit_answer_stating_no_lease_is_unavailable() {
    let net = SimulatedHttpNetwork::new();
    let _server = net.serve(laddr(), Arc::new(NoLease)).await.unwrap();
    let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
    let lease = LimiterLease::starting_at(Duration::from_secs(20));
    client.report_to(LimiterReports::new(&lease, B2buaMetrics::new()));
    assert_eq!(
        client.admit("c#k", 5, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Unavailable
    );
    assert_eq!(lease.current(), Duration::from_secs(20), "nothing learnt");
}

/// A limiter answering every request with one fixed body.
struct Answers(&'static str);

#[async_trait]
impl http_net::HttpService for Answers {
    async fn handle(&self, _: HttpRequest) -> HttpResponse {
        HttpResponse::ok(self.0.as_bytes().to_vec())
    }
}

async fn answering(body: &'static str) -> (HttpCallLimiter, Box<dyn http_net::HttpServerHandle>) {
    let net = SimulatedHttpNetwork::new();
    let server = net.serve(laddr(), Arc::new(Answers(body))).await.unwrap();
    (HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET), server)
}

#[tokio::test(start_paused = true)]
async fn a_lease_below_one_second_is_a_bad_body() {
    let admitted = r#"{"admitted":true,"released":false,"lease_ms":999}"#;
    let (client, _server) = answering(admitted).await;
    let lease = LimiterLease::starting_at(Duration::from_secs(20));
    let metrics = B2buaMetrics::new();
    client.report_to(LimiterReports::new(&lease, metrics.clone()));
    assert_eq!(
        client.admit("c#k", 6, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Unavailable
    );
    assert_eq!(lease.current(), Duration::from_secs(20), "nothing learnt");
    let bad = |op| metrics.limiter().failures_total(op, LimiterFailure::BadAnswer);
    assert_eq!(bad(LimiterOp::Admit), 1, "a bad answer");

    let (client, _server) = answering(r#"{"outcomes":["extended"],"lease_ms":0}"#).await;
    client.report_to(LimiterReports::new(&lease, metrics.clone()));
    let calls = [RefreshCall { key: "c#k".into(), held: held_of(&["x".into()]) }];
    assert_eq!(client.refresh(&calls).await, RefreshAnswer::Unavailable);
    assert_eq!(bad(LimiterOp::Refresh), 1);

    for body in [r#"{"calls":0,"lease_ms":999}"#, r#"{"calls":0}"#] {
        let (client, _server) = answering(body).await;
        client.report_to(LimiterReports::new(&lease, metrics.clone()));
        assert!(!client.health().unwrap().serving().await, "{body}");
    }
    assert_eq!(bad(LimiterOp::Health), 2);
    assert_eq!(lease.current(), Duration::from_secs(20), "nothing learnt");
}

/// Every request of `client`, one of each kind.
async fn one_of_each(client: &HttpCallLimiter) {
    client.admit("c#k", 7, &LimiterHeld::default(), &entries(), false).await;
    client.refresh(&[RefreshCall { key: "c#k".into(), held: held_of(&["x".into()]) }]).await;
    client.release(&["c#k".into()]).await;
    client.health().unwrap().serving().await;
}

/// A limiter answering every request with `status`.
struct Status(u16);

#[async_trait]
impl http_net::HttpService for Status {
    async fn handle(&self, _: HttpRequest) -> HttpResponse {
        HttpResponse::status(self.0)
    }
}

#[tokio::test(start_paused = true)]
async fn every_request_and_every_failure_is_counted_by_its_cause() {
    let ops = [LimiterOp::Admit, LimiterOp::Refresh, LimiterOp::Release, LimiterOp::Health];
    let (net, _server) = served().await;
    let client = HttpCallLimiter::new(Arc::new(net.clone()), laddr(), BUDGET);
    let lease = LimiterLease::starting_at(Duration::from_secs(20));
    let metrics = B2buaMetrics::new();
    client.report_to(LimiterReports::new(&lease, metrics.clone()));
    let m = metrics.limiter();

    one_of_each(&client).await;
    for op in ops {
        assert_eq!((m.requests_total(op), m.failures_of(op)), (1, 0), "{op:?} answered");
    }
    net.apply_fault(Fault::Stall { dst: laddr() });
    one_of_each(&client).await;
    net.apply_fault(Fault::Cut { dst: laddr() });
    one_of_each(&client).await;
    for op in ops {
        assert_eq!(m.requests_total(op), 3, "{op:?}");
        assert_eq!(m.failures_total(op, LimiterFailure::Timeout), 1, "{op:?} stalled");
        assert_eq!(m.failures_total(op, LimiterFailure::Transport), 1, "{op:?} cut");
    }

    let net = SimulatedHttpNetwork::new();
    let _refusing = net.serve(laddr(), Arc::new(Status(503))).await.unwrap();
    let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
    client.report_to(LimiterReports::new(&lease, metrics.clone()));
    one_of_each(&client).await;
    for op in ops {
        assert_eq!(m.failures_total(op, LimiterFailure::Status), 1, "{op:?} not 200");
        assert_eq!(m.failures_of(op), 3, "{op:?}");
    }
    assert_eq!(m.failures_total(LimiterOp::Admit, LimiterFailure::BreakerOpen), 0);
}

#[tokio::test(start_paused = true)]
async fn a_health_answer_states_the_lease() {
    let (client, _server) = answering(r#"{"calls":0,"lease_ms":200000}"#).await;
    let lease = LimiterLease::starting_at(Duration::from_secs(20));
    client.report_to(LimiterReports::new(&lease, B2buaMetrics::new()));
    assert!(client.health().unwrap().serving().await);
    assert_eq!(lease.current(), Duration::from_secs(200));
}

#[tokio::test(start_paused = true)]
async fn a_registration_whose_worker_is_gone_is_dropped() {
    let (net, _server) = served().await;
    let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
    for _ in 0..3 {
        let gone = LimiterLease::starting_at(Duration::from_secs(20));
        client.report_to(LimiterReports::new(&gone, B2buaMetrics::new()));
    }
    let lease = LimiterLease::starting_at(Duration::from_secs(20));
    client.report_to(LimiterReports::new(&lease, B2buaMetrics::new()));
    assert_eq!(client.endpoint.reports.read().unwrap().len(), 1, "only the live worker");
    assert_eq!(
        client.admit("c#k", 8, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Admitted
    );
    assert_eq!(lease.current(), Duration::from_secs(120));
}

#[tokio::test(start_paused = true)]
async fn an_admit_answer_contradicting_itself_teaches_no_lease() {
    let (client, _server) =
        answering(r#"{"admitted":false,"released":false,"lease_ms":5000}"#).await;
    let lease = LimiterLease::starting_at(Duration::from_secs(20));
    client.report_to(LimiterReports::new(&lease, B2buaMetrics::new()));
    assert_eq!(
        client.admit("c#k", 9, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Unavailable
    );
    assert_eq!(lease.current(), Duration::from_secs(20), "learnt only from a valid body");
}

#[tokio::test(start_paused = true)]
async fn a_client_at_an_address_always_has_it() {
    let (net, _server) = served().await;
    let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
    let health = client.health().unwrap();
    health.forget_address();
    assert!(health.has_address());
}

#[tokio::test(start_paused = true)]
async fn a_name_reaches_the_limiter_once_it_resolves() {
    let (net, _server) = served().await;
    let names = resolver();
    let target = ResolvedTarget::name_with("limiter:8080", names.clone());
    let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
    assert!(!client.health().unwrap().serving().await, "not resolving yet");
    names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
    assert!(client.health().unwrap().serving().await);
    assert_eq!(
        client.admit("c#k", 10, &LimiterHeld::default(), &entries(), false).await,
        AdmitOutcome::Admitted
    );
}
