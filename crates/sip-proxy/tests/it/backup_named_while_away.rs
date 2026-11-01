//! The backup a placed call's cookie names (ADR-0055 §8): the rendezvous
//! winner, by Call-ID, among the other workers read alive; when none is
//! alive, the winner among the other ordinals the proxy still knows — a
//! member whose endpoint is not ready, a member it does not read alive, a
//! departed address still tombstoned — and none only when it knows no other
//! ordinal at all. The primary is never its own backup.

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_proxy::addr::ProxyAddr;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::strategies::rendezvous::{rendezvous_select, RendezvousCandidate};
use sip_proxy::strategies::LoadBalancerStrategy;
use sip_proxy::strategy::{RoutingStrategy, SelectOpts};

const W1: &str = "b2b-1";
const W2: &str = "b2b-2";
const W3: &str = "b2b-3";
const A1: (&str, u16) = ("10.0.0.2", 5070);
const A2: (&str, u16) = ("10.0.0.3", 5070);
const A3: (&str, u16) = ("10.0.0.4", 5070);

/// The departed-address window of the proxy's tombstones: Timer H.
const TOMBSTONE_WINDOW: Duration = Duration::from_secs(32);

fn addr(t: (&str, u16)) -> ProxyAddr {
    ProxyAddr::new(t.0, t.1)
}

fn invite(call_id: &str) -> SipMessage {
    let branch = call_id.replace('@', ".");
    let raw = format!(
        "INVITE sip:bob@10.0.0.9:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{branch}\r\n\
From: <sip:alice@a>;tag=fromtag\r\n\
To: <sip:bob@b>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn strategy(reg: SimulatedWorkerRegistry, clock: Clock) -> LoadBalancerStrategy {
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    LoadBalancerStrategy::new(Arc::new(reg), hmac, observer, Arc::new(ProxyMetrics::new()), clock)
}

/// A pool of `workers`, every one read alive.
fn pool(workers: &[(&str, (&str, u16))], clock: Clock) -> SimulatedWorkerRegistry {
    SimulatedWorkerRegistry::with_clock(
        workers.iter().map(|(id, a)| WorkerEntry::alive(*id, addr(*a))).collect(),
        clock,
    )
}

/// The `(w_pri, w_bak)` the cookie of a new call `call_id` names: the call
/// placed by the strategy, then its stickiness encoded.
async fn placed(s: &LoadBalancerStrategy, call_id: &str) -> (String, String) {
    let msg = invite(call_id);
    let target = s.select_for_new_dialog(&msg, SelectOpts::default()).await.expect("a target");
    let cookie = s.encode_stickiness(&target, &msg).expect("a cookie");
    let field = |name: &str| cookie.get(name).cloned().unwrap_or_default();
    (field("w_pri"), field("w_bak"))
}

struct Key(&'static str);

impl RendezvousCandidate for Key {
    fn id(&self) -> &str {
        self.0
    }
}

/// The rendezvous winner for `call_id` among `ids`.
fn ranked_first(call_id: &str, ids: &[&'static str]) -> &'static str {
    let keys: Vec<Key> = ids.iter().map(|i| Key(i)).collect();
    rendezvous_select(call_id, &keys).expect("a candidate").0
}

#[tokio::test(start_paused = true)]
async fn the_other_alive_worker_is_the_backup() {
    let clock = Clock::test_at(0);
    let s = strategy(pool(&[(W1, A1), (W2, A2)], clock.clone()), clock);
    let (pri, bak) = placed(&s, "alive-1@h").await;
    assert_ne!(pri, bak);
    assert!(!bak.is_empty(), "the other alive worker is named");
}

#[tokio::test(start_paused = true)]
async fn a_member_whose_endpoint_is_not_ready_is_named() {
    let clock = Clock::test_at(0);
    let reg = pool(&[(W1, A1), (W2, A2)], clock.clone());
    reg.set_ready(W2, false);
    let s = strategy(reg, clock);
    assert_eq!(placed(&s, "not-ready-1@h").await, (W1.into(), W2.into()));
}

/// Past the tombstone of its address the not-ready member is still a member.
#[tokio::test(start_paused = true)]
async fn a_member_not_ready_past_its_tombstone_is_still_named() {
    let clock = Clock::test_at(0);
    let reg = pool(&[(W1, A1), (W2, A2)], clock.clone());
    reg.set_ready(W2, false);
    tokio::time::advance(TOMBSTONE_WINDOW + Duration::from_secs(1)).await;
    let s = strategy(reg, clock);
    assert_eq!(placed(&s, "not-ready-2@h").await, (W1.into(), W2.into()));
}

#[tokio::test(start_paused = true)]
async fn a_member_read_dead_or_unknown_is_named() {
    for health in [WorkerHealth::Dead, WorkerHealth::Unknown] {
        let clock = Clock::test_at(0);
        let reg = pool(&[(W1, A1), (W2, A2)], clock.clone());
        reg.set_health(W2, health);
        let s = strategy(reg, clock);
        assert_eq!(placed(&s, "not-alive-1@h").await, (W1.into(), W2.into()), "{health:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_departed_worker_is_named_while_its_address_is_tombstoned() {
    let clock = Clock::test_at(0);
    let reg = pool(&[(W1, A1), (W2, A2)], clock.clone());
    reg.remove(W2);
    let s = strategy(reg, clock);
    assert_eq!(placed(&s, "departed-1@h").await, (W1.into(), W2.into()));
}

#[tokio::test(start_paused = true)]
async fn a_departed_worker_whose_tombstone_closed_is_not_named() {
    let clock = Clock::test_at(0);
    let reg = pool(&[(W1, A1), (W2, A2)], clock.clone());
    reg.remove(W2);
    tokio::time::advance(TOMBSTONE_WINDOW + Duration::from_secs(1)).await;
    let s = strategy(reg, clock);
    assert_eq!(
        placed(&s, "departed-2@h").await,
        (W1.into(), String::new()),
        "the proxy knows no other ordinal: no backup"
    );
}

/// A primary re-created at a new address leaves its old address tombstoned
/// under its own ordinal: it is never named its own backup.
#[tokio::test(start_paused = true)]
async fn the_primary_is_never_its_own_backup() {
    let clock = Clock::test_at(0);
    let reg = pool(&[(W1, A1), (W2, A2)], clock.clone());
    reg.remove(W2);
    tokio::time::advance(TOMBSTONE_WINDOW + Duration::from_secs(1)).await;
    reg.set_address(W1, addr(("10.0.0.12", 5070)));
    let s = strategy(reg, clock);
    assert_eq!(placed(&s, "own-1@h").await, (W1.into(), String::new()));
}

#[tokio::test(start_paused = true)]
async fn an_alive_worker_is_named_before_any_away_one() {
    let clock = Clock::test_at(0);
    let reg = pool(&[(W1, A1), (W2, A2), (W3, A3)], clock.clone());
    reg.set_ready(W2, false);
    let s = strategy(reg, clock);
    for i in 0..20 {
        let (pri, bak) = placed(&s, &format!("one-alive-{i}@h")).await;
        let alive_other = if pri == W1 { W3 } else { W1 };
        assert_eq!(bak, alive_other, "call {i}: the alive worker, never the away one");
    }
}

/// With every other worker away, the call key ranks the ordinals the proxy
/// still knows: the same pick for the same Call-ID, spread over them.
#[tokio::test(start_paused = true)]
async fn with_every_other_worker_away_the_call_key_ranks_them() {
    let clock = Clock::test_at(0);
    let reg = pool(&[(W1, A1), (W2, A2), (W3, A3)], clock.clone());
    reg.set_ready(W1, false);
    reg.remove(W3);
    let s = strategy(reg, clock);
    let mut named = std::collections::BTreeSet::new();
    for i in 0..20 {
        let call_id = format!("none-alive-{i}@h");
        let (pri, bak) = placed(&s, &call_id).await;
        assert_eq!(pri, W2);
        assert_eq!(bak, ranked_first(&call_id, &[W1, W3]), "call {i}: the call key's pick");
        assert_eq!(placed(&s, &call_id).await, (pri, bak.clone()), "the same pick again");
        named.insert(bak);
    }
    assert_eq!(named.len(), 2, "the picks spread over both away ordinals: {named:?}");
}
