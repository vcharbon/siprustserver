//! A new call whose rendezvous winner is rate-capped (its per-worker AIMD
//! bucket empty) goes to the next rendezvous candidate that has a token; the
//! strategy answers `RateCapExhausted` only when no candidate has one
//! (ADR-0009 X6).

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_proxy::addr::ProxyAddr;
use sip_proxy::load_observer::{LoadObserverConfig, OverloadPayload, WorkerLoadObserver};
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::WorkerEntry;
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::strategies::LoadBalancerStrategy;
use sip_proxy::strategy::{RoutingStrategy, SelectError, SelectOpts};

const WORKERS: [(&str, (&str, u16)); 3] =
    [("b2b-1", ("10.0.0.2", 5070)), ("b2b-2", ("10.0.0.3", 5070)), ("b2b-3", ("10.0.0.4", 5070))];

fn addr(t: (&str, u16)) -> ProxyAddr {
    ProxyAddr::new(t.0, t.1)
}

fn id_at(target: &ProxyAddr) -> &'static str {
    WORKERS.iter().find(|(_, a)| addr(*a) == *target).map(|(id, _)| *id).unwrap()
}

/// An initial INVITE (no To-tag) or, with `to_tag`, an in-dialog one.
fn invite(call_id: &str, to_tag: Option<&str>) -> SipMessage {
    let to = to_tag.map_or("<sip:bob@b>".to_string(), |t| format!("<sip:bob@b>;tag={t}"));
    let branch = call_id.replace('@', ".");
    let raw = format!(
        "INVITE sip:bob@10.0.0.3:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{branch}\r\n\
From: <sip:alice@a>;tag=fromtag\r\n\
To: {to}\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

struct Fixture {
    strategy: LoadBalancerStrategy,
    observer: Arc<WorkerLoadObserver>,
    metrics: Arc<ProxyMetrics>,
}

/// `n` alive workers, each observed cool (so each has a bucket and none is
/// band-filtered), on a paused clock pinned at 0: a drained bucket refills by
/// a provable zero within the test.
fn fixture(n: usize) -> Fixture {
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let workers = WORKERS[..n].iter().map(|(id, a)| WorkerEntry::alive(*id, addr(*a))).collect();
    let reg = SimulatedWorkerRegistry::with_clock(workers, clock.clone());
    for (id, _) in &WORKERS[..n] {
        observer.apply_payload(id, &OverloadPayload { elu: 0.1, gc: 0.0, adm: 0.0 }, 0);
    }
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let metrics = Arc::new(ProxyMetrics::new());
    let strategy =
        LoadBalancerStrategy::new(Arc::new(reg), hmac, observer.clone(), metrics.clone(), clock);
    Fixture { strategy, observer, metrics }
}

impl Fixture {
    fn drain(&self, id: &str) {
        while self.observer.try_consume_for(id, 0).is_ok() {}
    }

    async fn select(&self, call_id: &str, to_tag: Option<&str>) -> Result<ProxyAddr, SelectError> {
        self.strategy.select_for_new_dialog(&invite(call_id, to_tag), SelectOpts::default()).await
    }

    /// The worker the call lands on while every bucket has tokens.
    async fn winner_of(&self, call_id: &str) -> &'static str {
        id_at(&self.select(call_id, None).await.unwrap())
    }
}

#[tokio::test(start_paused = true)]
async fn a_rate_capped_winner_spills_the_new_call_to_the_next_alive_worker() {
    let f = fixture(2);
    let winner = f.winner_of("call-spill@h").await;
    f.drain(winner);

    let target = f.select("call-spill@h", None).await;
    let landed = id_at(&target.expect("a worker with a token takes the call"));
    assert_ne!(landed, winner, "the rate-capped winner takes no new call");
    assert_eq!(f.metrics.lb_rate_cap_spilled_total(), 1);
    assert_eq!(f.metrics.overload_rejection_count("bucket_empty"), 0);
}

#[tokio::test(start_paused = true)]
async fn the_spill_follows_the_rendezvous_order() {
    // With three workers and the winner drained, each call lands where it
    // lands once the winner is out of the candidate set: the next HRW rank.
    for i in 0..16 {
        let call_id = format!("call-rank-{i}@h");
        let f = fixture(3);
        let winner = f.winner_of(&call_id).await;
        let second = {
            let g = fixture(3);
            // Taken out of the set by an above_critical band: the plain HRW
            // winner of the remaining two, no spill involved.
            g.observer.apply_payload(winner, &OverloadPayload { elu: 0.99, gc: 0.0, adm: 0.0 }, 0);
            g.winner_of(&call_id).await
        };
        f.drain(winner);

        assert_eq!(id_at(&f.select(&call_id, None).await.unwrap()), second, "{call_id}");
    }
}

#[tokio::test(start_paused = true)]
async fn every_candidate_rate_capped_is_refused_naming_the_winner() {
    let f = fixture(2);
    let winner = f.winner_of("call-full@h").await;
    for (id, _) in &WORKERS[..2] {
        f.drain(id);
    }

    match f.select("call-full@h", None).await {
        Err(SelectError::RateCapExhausted { worker_id, retry_after_sec }) => {
            assert_eq!(worker_id, winner);
            assert!(retry_after_sec >= 1, "a usable Retry-After");
        }
        other => panic!("expected RateCapExhausted, got {other:?}"),
    }
    assert_eq!(f.metrics.lb_rate_cap_spilled_total(), 0);
    assert_eq!(f.metrics.overload_rejection_count("bucket_empty"), 1);
}

#[tokio::test(start_paused = true)]
async fn an_in_dialog_request_stays_on_its_rate_capped_owner() {
    let f = fixture(2);
    let winner = f.winner_of("call-dlg@h").await;
    f.drain(winner);

    assert_eq!(id_at(&f.select("call-dlg@h", Some("bobtag")).await.unwrap()), winner);
    assert_eq!(f.metrics.lb_rate_cap_spilled_total(), 0);
}
