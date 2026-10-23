//! A request a stickiness cookie places goes by liveness alone (ADR-0050 §4,
//! §5): to the cookie's primary while it is alive, whatever its acceptance of
//! new calls and however recently the proxy read it alive; else to its backup
//! while alive; else nowhere — dropped, counted, never answered. A CANCEL and
//! a retransmission go where their transaction went while that node is alive,
//! and follow the cookie once it is not (RFC 3261 §9.1, §16.10).

use std::net::SocketAddr;
use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use crate::observability::metrics::NoTargetDrop;
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::{Acceptance, WorkerEntry, WorkerHealth};
use crate::security::hmac::{HmacKey, StaticHmacKeyProvider};
use crate::strategies::LoadBalancerStrategy;
use crate::strategy::{RouteParams, RoutingStrategy};
use crate::ProxyMetrics;

const W1_POD: &str = "10.244.5.8";
const W2_POD: &str = "10.244.5.9";
const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
const CALL_ID: &str = "dlg-1@10.244.7.13";

struct Fixture {
    core: ProxyCore,
    strategy: Arc<LoadBalancerStrategy>,
    registry: SimulatedWorkerRegistry,
    metrics: Arc<ProxyMetrics>,
}

/// A proxy over the load balancer and two workers, `w1` and `w2`, read as
/// `health` by the proxy.
async fn fixture_with(health: WorkerHealth) -> Fixture {
    let clock = Clock::test_at(0);
    let entry = |id: &str, host: &str| WorkerEntry {
        health,
        ..WorkerEntry::alive(id, ProxyAddr::new(host, 5060))
    };
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![entry("w1", W1_POD), entry("w2", W2_POD)],
        clock.clone(),
    );
    let metrics = Arc::new(ProxyMetrics::new());
    let strategy = Arc::new(LoadBalancerStrategy::new(
        Arc::new(registry.clone()),
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap()),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        metrics.clone(),
        clock.clone(),
    ));
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let core = ProxyCoreBuilder::for_test(
        ProxyAddr::new(PROXY_VIP, 5060),
        strategy.clone(),
        Arc::new(registry.clone()),
    )
    .clock(clock)
    .metrics(metrics.clone())
    .build(ep);
    Fixture { core, strategy, registry, metrics }
}

async fn fixture() -> Fixture {
    fixture_with(WorkerHealth::Alive).await
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn src() -> SocketAddr {
    format!("{UAC}:5060").parse().unwrap()
}

fn w1() -> Option<ProxyAddr> {
    Some(ProxyAddr::new(W1_POD, 5060))
}

fn w2() -> Option<ProxyAddr> {
    Some(ProxyAddr::new(W2_POD, 5060))
}

/// The caller's initial INVITE, `route` its Route header lines.
fn initial(method: &str, route: &str) -> SipMessage {
    parse(&format!(
        "{method} sip:bob@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKinv1;rport\r\n\
{route}\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=a\r\n\
To: <sip:bob@{UAC}>\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 {method}\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The proxy's own Record-Route URI carrying the cookie for `primary`.
fn cookie_uri(f: &Fixture, primary: &str) -> String {
    let params: RouteParams = f
        .strategy
        .encode_stickiness(&ProxyAddr::new(primary, 5060), &initial("INVITE", ""))
        .unwrap();
    let params: String = params.iter().map(|(k, v)| format!(";{k}={v}")).collect();
    format!("sip:{PROXY_VIP}:5060;lr{params}")
}

/// The caller's in-dialog `method` routed by the cookie, `branch` its Via
/// branch.
fn in_dialog(method: &str, cookie: &str, cseq: u32, branch: &str) -> SipMessage {
    parse(&format!(
        "{method} sip:bob@{W1_POD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Route: <{cookie}>\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=a\r\n\
To: <sip:bob@{UAC}>;tag=b\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: {cseq} {method}\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn bye(cookie: &str, cseq: u32) -> SipMessage {
    in_dialog("BYE", cookie, cseq, &format!("z9hG4bKbye{cseq}"))
}

fn renders(metrics: &ProxyMetrics, line: &str) -> bool {
    metrics.prometheus_text().contains(&format!("\n{line}\n"))
}

/// A primary that refuses new calls keeps its dialogs; nothing is promoted.
#[tokio::test]
async fn an_in_dialog_request_goes_to_its_primary_whatever_its_acceptance() {
    let f = fixture().await;
    let cookie = cookie_uri(&f, W1_POD);
    f.registry.set_acceptance("w1", Acceptance::Refuse);
    f.registry.set_acceptance("w2", Acceptance::Refuse);
    let out = f.core.route_request_from(&bye(&cookie, 3), src()).await;
    assert_eq!(out.target, w1());
}

/// A primary not alive, `Dead` or `Unknown`, passes the request to an alive
/// backup, refusing new calls or not.
#[tokio::test]
async fn an_in_dialog_request_goes_to_an_alive_backup_when_its_primary_is_not_alive() {
    let f = fixture().await;
    let cookie = cookie_uri(&f, W1_POD);
    f.registry.set_acceptance("w2", Acceptance::Refuse);
    for (cseq, health) in [(3, WorkerHealth::Dead), (4, WorkerHealth::Unknown)] {
        f.registry.set_health("w1", health);
        let out = f.core.route_request_from(&bye(&cookie, cseq), src()).await;
        assert_eq!(out.target, w2(), "{health:?} primary");
    }
}

/// Neither worker alive: the request is dropped and counted, never answered
/// and never placed afresh.
#[tokio::test]
async fn an_in_dialog_request_with_no_alive_worker_is_dropped_unanswered() {
    let f = fixture().await;
    let cookie = cookie_uri(&f, W1_POD);
    f.registry.set_health("w1", WorkerHealth::Dead);
    f.registry.set_health("w2", WorkerHealth::Unknown);
    let out = f.core.route_request_from(&bye(&cookie, 3), src()).await;
    assert_eq!(out.target, None);
    let m = &f.metrics;
    assert_eq!(m.no_target_dropped_count(NoTargetDrop::InDialog), 1);
    assert!(renders(m, "sip_proxy_no_target_dropped_total{kind=\"in_dialog\"} 1"));
    assert!(renders(m, "sip_messages_total{label=\"outbound:responded\"} 0"), "no answer");
    assert_eq!(m.reject_count("no_target_available"), 0);
}

/// A proxy that starts, or a worker that joins, reads it `Unknown`: its
/// dialogs go to the backup, and to it from its first `200` on, with no
/// window of their own.
#[tokio::test]
async fn an_in_dialog_request_goes_to_its_primary_from_the_first_alive_read() {
    let f = fixture_with(WorkerHealth::Unknown).await;
    f.registry.set_health("w2", WorkerHealth::Alive);
    let cookie = cookie_uri(&f, W1_POD);
    let out = f.core.route_request_from(&bye(&cookie, 3), src()).await;
    assert_eq!(out.target, w2(), "before its first 200: the backup");
    f.registry.set_health("w1", WorkerHealth::Alive);
    let out = f.core.route_request_from(&bye(&cookie, 4), src()).await;
    assert_eq!(out.target, w1(), "from its first 200 on: the primary");
}

/// A re-INVITE the backup relayed while the primary was read dead: its
/// CANCEL goes to the backup though the primary reads alive again.
#[tokio::test]
async fn a_cancel_goes_where_its_invite_went_while_that_node_is_alive() {
    let f = fixture().await;
    let cookie = cookie_uri(&f, W1_POD);
    f.registry.set_health("w1", WorkerHealth::Dead);
    let reinvite = in_dialog("INVITE", &cookie, 5, "z9hG4bKre5");
    assert_eq!(f.core.route_request_from(&reinvite, src()).await.target, w2());
    f.registry.set_health("w1", WorkerHealth::Alive);
    let cancel = in_dialog("CANCEL", &cookie, 5, "z9hG4bKre5");
    assert_eq!(f.core.route_request_from(&cancel, src()).await.target, w2());
}

/// The CANCEL of an INVITE whose node is no longer alive follows the
/// INVITE's cookie: to the backup, or dropped when no worker it names is
/// alive.
#[tokio::test]
async fn a_cancel_follows_the_cookie_once_its_invites_node_is_not_alive() {
    let f = fixture().await;
    let invite = initial("INVITE", "");
    let primary = f.core.route_request_from(&invite, src()).await.target.unwrap();
    let (primary_id, other_id, backup) =
        if primary.host == W1_POD { ("w1", "w2", w2()) } else { ("w2", "w1", w1()) };
    f.registry.set_health(primary_id, WorkerHealth::Dead);
    let cancel = initial("CANCEL", "");
    assert_eq!(f.core.route_request_from(&cancel, src()).await.target, backup);

    f.registry.set_health(other_id, WorkerHealth::Dead);
    assert_eq!(f.core.route_request_from(&cancel, src()).await.target, None);
    assert_eq!(f.metrics.no_target_dropped_count(NoTargetDrop::Cancel), 1);
}

/// The CANCEL of an INVITE whose node left the pool follows the cookie long
/// after the departed address's window closed, and when another worker is
/// bound at that address since.
#[tokio::test(start_paused = true)]
async fn a_cancel_follows_the_cookie_once_its_invites_node_left_the_pool() {
    for reused in [false, true] {
        let f = fixture().await;
        let invite = initial("INVITE", "");
        let went = f.core.route_request_from(&invite, src()).await.target.unwrap();
        let (primary_id, backup) =
            if Some(went.clone()) == w1() { ("w1", w2()) } else { ("w2", w1()) };
        f.registry.remove(primary_id);
        tokio::time::sleep(std::time::Duration::from_millis(
            crate::registry::tombstone::DEPARTED_ADDRESS_TTL_MS + 8_000,
        ))
        .await;
        if reused {
            f.registry.add(WorkerEntry::alive("w3", went.clone()));
        }
        let out = f.core.route_request_from(&initial("CANCEL", ""), src()).await;
        assert_eq!(out.target, backup, "reused={reused}: the backup holding the call");
    }
}

/// A caller's INVITE that preloads a plain Route to the proxy keeps the cookie
/// the proxy mints for it: its CANCEL, after the primary died, reaches the
/// backup (ADR-0014).
#[tokio::test]
async fn a_cancel_of_an_invite_with_a_plain_preloaded_route_follows_the_minted_cookie() {
    let f = fixture().await;
    let route = format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n");
    let primary = f.core.route_request_from(&initial("INVITE", &route), src()).await.target;
    let (primary_id, backup) = if primary == w1() { ("w1", w2()) } else { ("w2", w1()) };
    f.registry.set_health(primary_id, WorkerHealth::Dead);
    let out = f.core.route_request_from(&initial("CANCEL", &route), src()).await;
    assert_eq!(out.target, backup, "the CANCEL follows the minted cookie to the backup");
}

/// A retransmitted initial INVITE repeats its first copy's worker while it
/// is alive, and goes where the cookie it minted places the call once it is
/// not: to the backup holding the replicated call.
#[tokio::test]
async fn a_retransmitted_initial_invite_follows_its_minted_cookie_once_its_worker_is_dead() {
    let f = fixture().await;
    let invite = initial("INVITE", "");
    let first = f.core.route_request_from(&invite, src()).await.target;
    let (primary_id, backup) = if first == w1() { ("w1", w2()) } else { ("w2", w1()) };
    f.registry.set_acceptance(primary_id, Acceptance::Refuse);
    assert_eq!(f.core.route_request_from(&invite, src()).await.target, first, "alive: repeated");
    f.registry.set_health(primary_id, WorkerHealth::Dead);
    assert_eq!(f.core.route_request_from(&invite, src()).await.target, backup, "dead: the backup");
}
