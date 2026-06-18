//! Ports of the pure-logic `tests/sip-front-proxy/load-balancer/*` suite:
//! cookie encode/decode round-trip, HMAC tampering, the dead/not-ready/draining
//! routing matrix, the fresh-pod guard, unresolvable-id fallback, resharding
//! stickiness, and initial-health gating. No proxy SUT — these exercise the
//! `LoadBalancerStrategy` directly.

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_proxy::addr::ProxyAddr;
use sip_proxy::load_observer::{LoadObserverConfig, OverloadPayload, WorkerLoadObserver};
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::strategy::{DecodeResult, RoutingStrategy, SelectError, SelectOpts};
use sip_proxy::strategies::{LoadBalancerConfig, LoadBalancerStrategy};

const W1: &str = "b2b-1";
const W2: &str = "b2b-2";
const A1: (&str, u16) = ("10.0.0.2", 5070);
const A2: (&str, u16) = ("10.0.0.3", 5070);

fn addr(t: (&str, u16)) -> ProxyAddr {
    ProxyAddr::new(t.0, t.1)
}

/// Build a request with a given method / Call-ID / optional To-tag (in-dialog).
fn request(method: &str, call_id: &str, to_tag: Option<&str>) -> SipMessage {
    request_with_rph(method, call_id, to_tag, None)
}

/// As [`request`], optionally stamping a `Resource-Priority` header (the on-wire
/// emergency signal `is_emergency_invite` detects, mirroring the TS
/// `buildInvite(callId, true)` which adds `Resource-Priority: esnet.0`).
fn request_with_rph(method: &str, call_id: &str, to_tag: Option<&str>, rph: Option<&str>) -> SipMessage {
    let to = match to_tag {
        Some(t) => format!("<sip:bob@b>;tag={t}"),
        None => "<sip:bob@b>".to_string(),
    };
    let rph_line = match rph {
        Some(v) => format!("Resource-Priority: {v}\r\n"),
        None => String::new(),
    };
    let raw = format!(
        "{method} sip:bob@10.0.0.3:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{call_id}\r\n\
From: <sip:alice@a>;tag=fromtag\r\n\
To: {to}\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 {method}\r\n\
Max-Forwards: 70\r\n\
{rph_line}\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

/// An emergency INVITE (`Resource-Priority: esnet.0`), the on-wire signal.
fn emergency_invite(call_id: &str) -> SipMessage {
    request_with_rph("INVITE", call_id, None, Some("esnet.0"))
}

fn strategy(reg: SimulatedWorkerRegistry, clock: Clock) -> LoadBalancerStrategy {
    let hmac = Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let metrics = Arc::new(ProxyMetrics::new());
    LoadBalancerStrategy::new(Arc::new(reg), hmac, observer, metrics, clock, LoadBalancerConfig::default())
}

fn strategy_with_observer(
    reg: SimulatedWorkerRegistry,
    clock: Clock,
    observer: Arc<WorkerLoadObserver>,
) -> LoadBalancerStrategy {
    let hmac = Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let metrics = Arc::new(ProxyMetrics::new());
    LoadBalancerStrategy::new(Arc::new(reg), hmac, observer, metrics, clock, LoadBalancerConfig::default())
}

fn two_worker_registry(clock: Clock) -> SimulatedWorkerRegistry {
    SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry::alive(W1, addr(A1)), WorkerEntry::alive(W2, addr(A2))],
        clock,
    )
}

#[tokio::test]
async fn hmac_tampering_rejected() {
    let reg = two_worker_registry(Clock::test_at(0));
    let s = strategy(reg, Clock::test_at(0));
    let invite = request("INVITE", "call-tamper@h", None);
    let target = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let mut params = s.encode_stickiness(&target, &invite).unwrap();

    // Pin the v3 cookie wire format the proxy stamped (mirrors the TS RR-shape
    // assertions: v=3, e=0, w_pri/w_bak/kid/sig present).
    assert_eq!(params.get("v").unwrap(), "3");
    assert_eq!(params.get("e").unwrap(), "0", "non-emergency INVITE ⇒ e=0");
    assert!(!params.get("w_pri").unwrap().is_empty(), "w_pri present");
    assert!(params.contains_key("w_bak"), "w_bak present (may be empty)");
    assert!(!params.get("kid").unwrap().is_empty(), "kid present");
    assert!(!params.get("sig").unwrap().is_empty(), "sig present");

    // Flip ONE base64url char (length-preserving) so the truncated MAC stays
    // 16 bytes — this exercises the verify-MISMATCH reject branch specifically,
    // not the length!=16 "malformed" branch. Decode must Reject 403.
    let sig = params.get("sig").unwrap().clone();
    let first = sig.chars().next().expect("non-empty sig");
    let replacement = if first == 'A' { 'B' } else { 'A' };
    let tampered: String = std::iter::once(replacement).chain(sig.chars().skip(1)).collect();
    assert_eq!(tampered.len(), sig.len(), "single-char flip preserves length");
    assert_ne!(tampered, sig, "flip actually changed the signature");
    params.insert("sig".into(), tampered);
    let bye = request("BYE", "call-tamper@h", Some("bobtag"));
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::Reject { status, reason } => {
            assert_eq!(status, 403);
            assert!(reason.contains("mismatch"), "expected MAC-mismatch reject, got: {reason}");
        }
        other => panic!("expected Reject 403, got {other:?}"),
    }
}

#[tokio::test]
async fn cookie_round_trips_to_alive_primary() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg, clock);
    let invite = request("INVITE", "call-rt@h", None);
    let target = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&target, &invite).unwrap();
    assert_eq!(params.get("v").unwrap(), "3");

    let bye = request("BYE", "call-rt@h", Some("bobtag"));
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::Forward { target: t, .. } => assert_eq!(t, target),
        other => panic!("expected Forward to primary, got {other:?}"),
    }
}

#[tokio::test]
async fn emergency_cookie_round_trips_e1_and_is_emergency() {
    // The literal item-07 surface: the v3 `e=1` emergency flag must survive an
    // encode → decode round-trip. Mirrors the TS `e=1` contract (stickinessInput
    // emergency arg; decodeStickiness isEmergencyDialog = emergencyFlag === '1').
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg, clock);

    // (a) ENCODE: an emergency INVITE (`Resource-Priority: esnet.0`) mints a
    //     cookie whose encoded params carry `e=1` (and stays v=3).
    let invite = emergency_invite("call-em-rt@h");
    let target = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&target, &invite).unwrap();
    assert_eq!(params.get("v").unwrap(), "3");
    assert_eq!(params.get("e").unwrap(), "1", "emergency INVITE ⇒ e=1");

    // (b) DECODE: an in-dialog request carrying that `e=1` cookie yields a
    //     DecodeResult whose `is_emergency` flag is true (here a Forward to the
    //     still-alive primary).
    let bye = request("BYE", "call-em-rt@h", Some("bobtag"));
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::Forward { target: t, is_emergency } => {
            assert_eq!(t, target);
            assert!(is_emergency, "e=1 cookie must decode to is_emergency: true");
        }
        other => panic!("expected Forward(is_emergency=true) to primary, got {other:?}"),
    }
}

#[tokio::test]
async fn cookie_route_fallback_when_primary_dead() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg.clone(), clock);
    let invite = request("INVITE", "call-fb@h", None);
    let primary = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&primary, &invite).unwrap();
    let primary_id = params.get("w_pri").unwrap().clone();
    let backup_id = params.get("w_bak").unwrap().clone();
    assert!(!backup_id.is_empty(), "two-worker cluster should name a backup");

    // Primary dies; in-dialog BYE must route to the cookie's alive backup.
    reg.set_health(&primary_id, WorkerHealth::Dead);
    let backup_addr = reg.resolve(&backup_id).unwrap().address;
    let bye = request("BYE", "call-fb@h", Some("bobtag"));
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::ForwardBackup { target, .. } => assert_eq!(target, backup_addr),
        other => panic!("expected ForwardBackup, got {other:?}"),
    }
    // ACK after death also follows the backup (dead primary ⇒ no alive-ACK exemption).
    let ack = request("ACK", "call-fb@h", Some("bobtag"));
    assert!(matches!(s.decode_stickiness(&params, &ack).await, DecodeResult::ForwardBackup { .. }));
}

#[tokio::test]
async fn decode_forward_not_ready_promotes_to_backup() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg.clone(), clock);
    let invite = request("INVITE", "call-nr@h", None);
    let primary = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&primary, &invite).unwrap();
    let primary_id = params.get("w_pri").unwrap().clone();

    reg.set_health(&primary_id, WorkerHealth::NotReady);
    let bye = request("BYE", "call-nr@h", Some("bobtag"));
    assert!(matches!(s.decode_stickiness(&params, &bye).await, DecodeResult::ForwardBackup { .. }));
}

#[tokio::test]
async fn unresolvable_primary_falls_back_to_backup() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg.clone(), clock);
    let invite = request("INVITE", "call-un@h", None);
    let primary = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&primary, &invite).unwrap();
    let primary_id = params.get("w_pri").unwrap().clone();

    // Primary scaled down entirely → the cookie's (alive) backup serves it.
    reg.remove(&primary_id);
    let bye = request("BYE", "call-un@h", Some("bobtag"));
    assert!(matches!(s.decode_stickiness(&params, &bye).await, DecodeResult::ForwardBackup { .. }));
}

#[tokio::test]
async fn draining_grace_keeps_primary_then_falls_back() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg.clone(), clock.clone());
    let invite = request("INVITE", "call-dr@h", None);
    let primary = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&primary, &invite).unwrap();
    let primary_id = params.get("w_pri").unwrap().clone();

    // Drain the primary at t=0 (draining_since stamped 0). Within the 5 s grace,
    // an in-dialog re-INVITE stays on the primary.
    reg.set_health(&primary_id, WorkerHealth::Draining);
    let reinvite = request("INVITE", "call-dr@h", Some("bobtag"));
    assert!(matches!(s.decode_stickiness(&params, &reinvite).await, DecodeResult::Forward { .. }));
}

#[tokio::test(start_paused = true)]
async fn draining_post_grace_falls_back_to_backup() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg.clone(), clock.clone());
    let invite = request("INVITE", "call-dg@h", None);
    let primary = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&primary, &invite).unwrap();
    let primary_id = params.get("w_pri").unwrap().clone();
    reg.set_health(&primary_id, WorkerHealth::Draining);

    // Past the 5 s grace window: in-dialog request promotes to the backup.
    tokio::time::advance(std::time::Duration::from_millis(5_001)).await;
    let reinvite = request("INVITE", "call-dg@h", Some("bobtag"));
    assert!(matches!(s.decode_stickiness(&params, &reinvite).await, DecodeResult::ForwardBackup { .. }));
}

#[tokio::test(start_paused = true)]
async fn fresh_pod_guard_promotes_then_relaxes() {
    let clock = Clock::test_at(0);
    // Primary is freshly-spawned (first_seen_at_ms = 0); backup is not.
    let reg = SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry { first_seen_at_ms: Some(0), ..WorkerEntry::alive(W1, addr(A1)) },
            WorkerEntry::alive(W2, addr(A2)),
        ],
        clock.clone(),
    );
    let s = strategy(reg.clone(), clock.clone());
    let invite = request("INVITE", "call-fp@h", None);
    // Force the cookie to name W1 primary by encoding for W1's address.
    let params = s.encode_stickiness(&addr(A1), &invite).unwrap();
    assert_eq!(params.get("w_pri").unwrap(), W1);

    // Inside the 20 s guard, an in-dialog BYE to a fresh primary promotes to backup.
    let bye = request("BYE", "call-fp@h", Some("bobtag"));
    assert!(matches!(s.decode_stickiness(&params, &bye).await, DecodeResult::ForwardBackup { .. }));

    // Past the guard, it forwards to the (now-trusted) primary.
    tokio::time::advance(std::time::Duration::from_millis(20_001)).await;
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::Forward { target, .. } => assert_eq!(target, addr(A1)),
        other => panic!("expected Forward to primary post-guard, got {other:?}"),
    }
}

#[tokio::test]
async fn initial_health_unknown_workers_are_not_selectable() {
    let clock = Clock::test_at(0);
    let reg = SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry { health: WorkerHealth::Unknown, ..WorkerEntry::alive(W1, addr(A1)) }],
        clock.clone(),
    );
    let s = strategy(reg.clone(), clock);
    let invite = request("INVITE", "call-ih@h", None);
    assert!(matches!(
        s.select_for_new_dialog(&invite, SelectOpts::default()).await,
        Err(SelectError::NoTarget { .. })
    ));
    // After it goes alive, it is selectable.
    reg.set_health(W1, WorkerHealth::Alive);
    assert_eq!(s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap(), addr(A1));
}

#[tokio::test]
async fn above_critical_band_filtered_for_non_emergency_only() {
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    // Single worker, pinned above_critical.
    let reg = SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W1, addr(A1))], clock.clone());
    observer.apply_payload(W1, &OverloadPayload { elu: 0.95, gc: 0.0, adm: 0.0 });
    let s = strategy_with_observer(reg, clock, observer);

    // Non-emergency new dialog is filtered out → NoTarget.
    let invite = request("INVITE", "call-ov@h", None);
    assert!(matches!(
        s.select_for_new_dialog(&invite, SelectOpts::default()).await,
        Err(SelectError::NoTarget { .. })
    ));
    // Emergency bypasses the band filter — two independent paths, one assertion each:
    // (a) an on-wire `Resource-Priority: esnet.0` INVITE drives `is_emergency_invite`
    //     on the select path directly (mirrors the TS `buildInvite(callId, true)`).
    let rph_invite = emergency_invite("call-ov-rph@h");
    assert_eq!(
        s.select_for_new_dialog(&rph_invite, SelectOpts::default()).await.unwrap(),
        addr(A1)
    );
    // (b) the explicit `emergency_override` opts shortcut (used by the dispatcher-marked
    //     in-dialog path) bypasses the filter on a non-RPH INVITE.
    let emergency = SelectOpts { emergency_override: true };
    assert_eq!(s.select_for_new_dialog(&invite, emergency).await.unwrap(), addr(A1));
}

#[tokio::test]
async fn resharding_keeps_in_dialog_on_cookie_primary() {
    // Stickiness wins over re-shard: a cookie minted for the original primary
    // still decodes to it after a third worker joins.
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg.clone(), clock);
    let invite = request("INVITE", "call-rs@h", None);
    let primary = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&primary, &invite).unwrap();

    reg.add(WorkerEntry::alive("b2b-3", ProxyAddr::new("10.0.0.4", 5070)));
    let bye = request("BYE", "call-rs@h", Some("bobtag"));
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::Forward { target, .. } => assert_eq!(target, primary),
        other => panic!("expected stickiness to original primary, got {other:?}"),
    }
}
