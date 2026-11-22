//! How the load balancer routes around a `Slow` worker (ADR-0069): its
//! dialogs still reach it by their cookie, as primary or as backup; a new
//! normal dialog never does, and an emergency one does only when no worker is
//! `Alive`. No proxy SUT: these drive the `LoadBalancerStrategy` directly.

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_proxy::addr::ProxyAddr;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::strategies::LoadBalancerStrategy;
use sip_proxy::strategy::{DecodeResult, RoutingStrategy, SelectError, SelectOpts};

const W1: &str = "b2b-1";
const W2: &str = "b2b-2";

fn a1() -> ProxyAddr {
    ProxyAddr::new("192.0.2.2", 5070)
}

fn a2() -> ProxyAddr {
    ProxyAddr::new("192.0.2.3", 5070)
}

fn request(method: &str, call_id: &str, to_tag: Option<&str>, rph: Option<&str>) -> SipMessage {
    let to = to_tag.map_or("<sip:bob@b>".to_string(), |t| format!("<sip:bob@b>;tag={t}"));
    let rph = rph.map_or(String::new(), |v| format!("Resource-Priority: {v}\r\n"));
    let branch = call_id.replace('@', ".");
    let raw = format!(
        "{method} sip:bob@192.0.2.3:5070 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 192.0.2.9:5060;branch=z9hG4bK-{branch}\r\n\
From: <sip:alice@a>;tag=fromtag\r\n\
To: {to}\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 {method}\r\n\
Max-Forwards: 70\r\n\
{rph}\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::new().parse(raw.as_bytes()).unwrap()
}

fn rig() -> (SimulatedWorkerRegistry, LoadBalancerStrategy) {
    let clock = Clock::test_at(0);
    let reg = SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry::alive(W1, a1()), WorkerEntry::alive(W2, a2())],
        clock.clone(),
    );
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let s = LoadBalancerStrategy::new(
        Arc::new(reg.clone()),
        hmac,
        observer,
        Arc::new(ProxyMetrics::new()),
        clock,
    );
    (reg, s)
}

/// A dialog whose primary reads `Slow` stays on it; once the primary is
/// `Dead`, a `Slow` backup takes it; with the backup `Dead` too it is
/// unroutable.
#[test]
fn a_slow_worker_serves_its_dialogs_as_primary_and_as_backup() {
    let (reg, s) = rig();
    let params = s.encode_stickiness(&a1(), &request("INVITE", "call-slow@h", None, None)).unwrap();
    assert_eq!(params.get("w_bak").map(String::as_str), Some(W2));
    let bye = request("BYE", "call-slow@h", Some("bobtag"), None);

    reg.set_health(W1, WorkerHealth::Slow);
    assert_eq!(
        s.decode_stickiness(&params, &bye),
        DecodeResult::Forward { target: a1(), is_emergency: false }
    );
    reg.set_health(W1, WorkerHealth::Dead);
    reg.set_health(W2, WorkerHealth::Slow);
    assert_eq!(
        s.decode_stickiness(&params, &bye),
        DecodeResult::ForwardBackup { target: a2(), is_emergency: false }
    );
    reg.set_health(W2, WorkerHealth::Dead);
    assert_eq!(s.decode_stickiness(&params, &bye), DecodeResult::Unroutable);
}

/// A new normal dialog goes to the `Alive` worker whichever the rank names,
/// and finds no target with every worker `Slow`; an emergency INVITE is placed
/// on a `Slow` worker then, and on the `Alive` one while one is.
#[test]
fn a_slow_worker_takes_no_new_dialog_but_an_emergency_one_with_none_alive() {
    let (reg, s) = rig();
    reg.set_health(W1, WorkerHealth::Slow);
    for n in 0..32 {
        let call = format!("call-new-{n}@h");
        let invite = request("INVITE", &call, None, None);
        assert_eq!(s.select_for_new_dialog(&invite, SelectOpts::default()).ok(), Some(a2()));
        let emergency = request("INVITE", &call, None, Some("esnet.0"));
        assert_eq!(s.select_for_new_dialog(&emergency, SelectOpts::default()).ok(), Some(a2()));
    }
    reg.set_health(W2, WorkerHealth::Slow);
    let invite = request("INVITE", "call-none@h", None, None);
    assert!(matches!(
        s.select_for_new_dialog(&invite, SelectOpts::default()),
        Err(SelectError::NoTarget { .. })
    ));
    let emergency = request("INVITE", "call-none@h", None, Some("esnet.0"));
    let placed = s.select_for_new_dialog(&emergency, SelectOpts::default()).unwrap();
    assert!(placed == a1() || placed == a2(), "{placed:?}");
    reg.set_health(W1, WorkerHealth::Dead);
    reg.set_health(W2, WorkerHealth::Dead);
    assert!(matches!(
        s.select_for_new_dialog(&emergency, SelectOpts::default()),
        Err(SelectError::NoTarget { .. })
    ));
}
