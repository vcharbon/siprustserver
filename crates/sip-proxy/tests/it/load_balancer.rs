//! Ports of the pure-logic `tests/sip-front-proxy/load-balancer/*` suite:
//! cookie encode/decode round-trip, HMAC tampering, the liveness routing
//! matrix and the acceptance of new calls (ADR-0050), unresolvable-id
//! fallback, resharding stickiness, and initial-health gating. No proxy SUT —
//! these exercise the `LoadBalancerStrategy` directly.

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_proxy::addr::ProxyAddr;
use sip_proxy::load_observer::{EluBand, LoadObserverConfig, OverloadPayload, WorkerLoadObserver};
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{Acceptance, WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::strategies::LoadBalancerStrategy;
use sip_proxy::strategy::{DecodeResult, RoutingStrategy, SelectError, SelectOpts};

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
fn request_with_rph(
    method: &str,
    call_id: &str,
    to_tag: Option<&str>,
    rph: Option<&str>,
) -> SipMessage {
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
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let metrics = Arc::new(ProxyMetrics::new());
    LoadBalancerStrategy::new(Arc::new(reg), hmac, observer, metrics, clock)
}

fn strategy_with_observer(
    reg: SimulatedWorkerRegistry,
    clock: Clock,
    observer: Arc<WorkerLoadObserver>,
) -> LoadBalancerStrategy {
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let metrics = Arc::new(ProxyMetrics::new());
    LoadBalancerStrategy::new(Arc::new(reg), hmac, observer, metrics, clock)
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
    // The v3 `e=1` emergency flag must survive an
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
        DecodeResult::Forward { target: t, is_emergency, .. } => {
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

/// A primary refusing new calls keeps every in-dialog request: acceptance is
/// never read by the decode (ADR-0050 §4).
#[tokio::test]
async fn an_in_dialog_request_goes_to_a_refusing_primary() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg.clone(), clock);
    let params = s.encode_stickiness(&addr(A1), &request("INVITE", "call-rf@h", None)).unwrap();
    reg.set_acceptance(W1, Acceptance::Refuse);
    reg.set_acceptance(W2, Acceptance::Refuse);
    for method in ["BYE", "INVITE", "ACK", "CANCEL"] {
        let req = request(method, "call-rf@h", Some("bobtag"));
        let decoded = s.decode_stickiness(&params, &req).await;
        assert_eq!(decoded, DecodeResult::Forward { target: addr(A1), is_emergency: false });
    }
}

/// A primary not alive passes the request to the backup only while the
/// backup is alive; with neither alive the request is unroutable, never
/// placed afresh (ADR-0050 §4, §5).
#[tokio::test]
async fn an_in_dialog_request_takes_an_alive_backup_and_nothing_else() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg.clone(), clock);
    let params = s.encode_stickiness(&addr(A1), &request("INVITE", "call-nb@h", None)).unwrap();
    let bye = request("BYE", "call-nb@h", Some("bobtag"));
    let backup = DecodeResult::ForwardBackup { target: addr(A2), is_emergency: false };
    for primary in [WorkerHealth::Dead, WorkerHealth::Unknown] {
        reg.set_health(W1, primary);
        reg.set_health(W2, WorkerHealth::Alive);
        assert_eq!(s.decode_stickiness(&params, &bye).await, backup, "{primary:?} primary");
        for not_alive in [WorkerHealth::Dead, WorkerHealth::Unknown] {
            reg.set_health(W2, not_alive);
            assert_eq!(
                s.decode_stickiness(&params, &bye).await,
                DecodeResult::Unroutable,
                "{primary:?} primary, {not_alive:?} backup"
            );
        }
    }
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

/// A primary read alive is served from that read on, however recently it
/// joined: the registry holds no join instant (ADR-0050 §8).
#[tokio::test]
async fn a_primary_read_alive_is_served_at_once_after_it_joined() {
    let clock = Clock::test_at(0);
    let reg =
        SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W2, addr(A2))], clock.clone());
    let s = strategy(reg.clone(), clock);
    reg.add(WorkerEntry { health: WorkerHealth::Unknown, ..WorkerEntry::alive(W1, addr(A1)) });
    let params = s.encode_stickiness(&addr(A1), &request("INVITE", "call-jn@h", None)).unwrap();
    let bye = request("BYE", "call-jn@h", Some("bobtag"));
    let backup = DecodeResult::ForwardBackup { target: addr(A2), is_emergency: false };
    assert_eq!(s.decode_stickiness(&params, &bye).await, backup, "unknown: the backup");
    reg.set_health(W1, WorkerHealth::Alive);
    let primary = DecodeResult::Forward { target: addr(A1), is_emergency: false };
    assert_eq!(s.decode_stickiness(&params, &bye).await, primary, "alive: the primary");
}

/// The cookie's backup is chosen among the alive workers accepting new
/// calls; when none but the primary accepts, among every alive one, so a
/// cluster refusing everywhere still replicates (ADR-0050 §7).
#[tokio::test]
async fn the_backup_is_chosen_among_accepting_workers_else_among_alive_ones() {
    const W3: &str = "b2b-3";
    let a3 = ("10.0.0.4", 5070);
    let clock = Clock::test_at(0);
    let reg = SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry::alive(W1, addr(A1)),
            WorkerEntry::alive(W2, addr(A2)),
            WorkerEntry::alive(W3, addr(a3)),
        ],
        clock.clone(),
    );
    let s = strategy(reg.clone(), clock);
    reg.set_acceptance(W2, Acceptance::Refuse);
    for n in 0..16 {
        let invite = request("INVITE", &format!("call-bk-{n}@h"), None);
        let params = s.encode_stickiness(&addr(A1), &invite).unwrap();
        assert_eq!(params.get("w_bak").unwrap(), W3, "call {n}: the accepting worker");
    }
    reg.set_acceptance(W3, Acceptance::Refuse);
    reg.set_health(W2, WorkerHealth::Dead);
    for n in 0..16 {
        let invite = request("INVITE", &format!("call-bk-{n}@h"), None);
        let params = s.encode_stickiness(&addr(A1), &invite).unwrap();
        assert_eq!(params.get("w_bak").unwrap(), W3, "call {n}: the alive refusing worker");
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

/// No alive worker accepts: a new dialog goes to an alive refusing one,
/// which answers it itself (ADR-0050 §6); a dead worker takes nothing.
#[tokio::test]
async fn a_new_dialog_falls_back_to_a_refusing_worker_when_none_accepts() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    reg.set_health(W1, WorkerHealth::Dead);
    reg.set_acceptance(W2, Acceptance::Refuse);
    let s = strategy(reg, clock);
    for n in 0..8 {
        let invite = request("INVITE", &format!("call-rf-fallback-{n}@h"), None);
        let selected = s.select_for_new_dialog(&invite, SelectOpts::default()).await;
        assert_eq!(selected.ok(), Some(addr(A2)), "call {n}: the refusing worker takes it");
    }
}

/// While one alive worker accepts, no new dialog goes to a refusing one.
#[tokio::test]
async fn a_new_dialog_skips_a_refusing_worker_while_one_accepts() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    reg.set_acceptance(W2, Acceptance::Refuse);
    let s = strategy(reg, clock);
    for n in 0..32 {
        let invite = request("INVITE", &format!("call-rf-skip-{n}@h"), None);
        let selected = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
        assert_eq!(selected, addr(A1), "call {n} goes to the accepting worker");
    }
}

#[tokio::test]
async fn above_critical_band_filtered_for_non_emergency_only() {
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    // Single worker, pinned above_critical.
    let reg =
        SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W1, addr(A1))], clock.clone());
    observer.apply_payload(W1, &OverloadPayload { elu: 0.95, gc: 0.0, adm: 0.0 }, clock.now_ms());
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
    let emergency = SelectOpts { emergency_override: true, ..SelectOpts::default() };
    assert_eq!(s.select_for_new_dialog(&invite, emergency).await.unwrap(), addr(A1));
}

#[tokio::test(start_paused = true)]
async fn rate_cap_exhausted_when_winners_bucket_is_empty() {
    // Wires the per-worker AIMD bucket into admission (TS LoadBalancer.ts:335-348):
    // a non-emergency new dialog whose rendezvous winner has a drained bucket is
    // rejected with `RateCapExhausted`, and re-INVITE / emergency on the SAME
    // worker bypass the bucket and are still admitted.
    //
    // `start_paused` freezes `tokio::time`, so every `clock.now_ms()` — the drain
    // loop's, AND the strategy's internal read inside `select_for_new_dialog` —
    // returns the SAME instant (anchor 0 + 0 elapsed). The bucket therefore refills
    // by a provable zero between the drain and the assert, matching the TS unit
    // test's pinned `nowMs=0`. Under a plain (unpaused) `#[tokio::test]` the real
    // monotonic clock ticks between the two and a token can trickle back under
    // extreme scheduling delay — a latent CI flake this removes.
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    // Single worker so the rendezvous winner is deterministic; seed a cool
    // payload (below_soft) so the bucket exists and is NOT band-filtered, then
    // drain every token at the same instant.
    let reg =
        SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W1, addr(A1))], clock.clone());
    observer.apply_payload(W1, &OverloadPayload { elu: 0.1, gc: 0.0, adm: 0.0 }, clock.now_ms());
    while observer.try_consume_for(W1, clock.now_ms()).is_ok() {}
    let s = strategy_with_observer(reg, clock, observer);

    // Non-emergency new dialog on the empty bucket → 503-bound RateCapExhausted
    // with a finite, non-zero Retry-After (the real per-bucket value, >= 1).
    let invite = request("INVITE", "call-rc@h", None);
    match s.select_for_new_dialog(&invite, SelectOpts::default()).await {
        Err(SelectError::RateCapExhausted { worker_id, retry_after_sec }) => {
            assert_eq!(worker_id, W1);
            assert!(retry_after_sec >= 1, "Retry-After must be a usable, non-zero value");
        }
        other => panic!("expected RateCapExhausted, got {other:?}"),
    }

    // In-dialog re-INVITE (To-tag present) bypasses the bucket — still admitted
    // despite the empty bucket (AIMD is a new-call knob only).
    let reinvite = request("INVITE", "call-rc@h", Some("bobtag"));
    assert_eq!(s.select_for_new_dialog(&reinvite, SelectOpts::default()).await.unwrap(), addr(A1));

    // Emergency new dialog also bypasses the bucket.
    let emergency = request("INVITE", "call-rc-em@h", None);
    let opts = SelectOpts { emergency_override: true, ..SelectOpts::default() };
    assert_eq!(s.select_for_new_dialog(&emergency, opts).await.unwrap(), addr(A1));
}

#[tokio::test(start_paused = true)]
async fn a_dialog_admitted_as_new_meets_the_empty_bucket() {
    // A request naming a dialog no verified cookie backs (`admit_as_new`) is a
    // new call: the bucket that an in-dialog request bypasses refuses it.
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let reg =
        SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W1, addr(A1))], clock.clone());
    observer.apply_payload(W1, &OverloadPayload { elu: 0.1, gc: 0.0, adm: 0.0 }, clock.now_ms());
    while observer.try_consume_for(W1, clock.now_ms()).is_ok() {}
    let s = strategy_with_observer(reg, clock, observer);

    let reinvite = request("INVITE", "call-unbacked@h", Some("forged"));
    let opts = SelectOpts { admit_as_new: true, ..SelectOpts::default() };
    let refused = s.select_for_new_dialog(&reinvite, opts).await;
    assert!(matches!(refused, Err(SelectError::RateCapExhausted { .. })), "{refused:?}");
}

#[tokio::test]
async fn unobserved_worker_is_admitted_without_a_bucket() {
    // Bootstrap-friendly: a worker the LB has never seen a payload from has no
    // bucket, so `try_consume_for` admits it (no rate cap before first OPTIONS).
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg, clock); // fresh observer, no payloads applied
    let invite = request("INVITE", "call-boot@h", None);
    // Succeeds (lands on one of the two alive workers) — no RateCapExhausted.
    let target = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    assert!(target == addr(A1) || target == addr(A2));
}

// ── in-dialog stickiness under CPU starvation ───────────────────────────────
//
// The emergency-under-starvation work makes the LB shed NEW non-emergency far
// more aggressively (a CPU-starved worker lands in AboveCritical and its bucket
// drains). These tests PIN the invariant that already-established (in-dialog)
// traffic is NEVER caught by that shedding — an in-dialog request must stay on
// its owning worker even when that worker is fully loaded, so an established
// call is not torn down by overload protection. Both the band filter
// (select path) and the AIMD token bucket are new-call knobs only.

#[tokio::test(start_paused = true)]
async fn in_dialog_sticks_to_owner_in_above_critical_and_bypasses_empty_bucket() {
    // Worst case under starve: the owning worker is BOTH pinned AboveCritical AND
    // its token bucket is drained. An in-dialog request (To-tag present) must
    // still select that worker — neither the AboveCritical candidate filter nor
    // the empty bucket may shed it.
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    // Single worker so the rendezvous winner is deterministic.
    let reg =
        SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W1, addr(A1))], clock.clone());
    // Pin AboveCritical (elu 0.95 > the new 0.65 critical threshold) so the
    // worker WOULD be filtered out of NON-emergency new-dialog candidates…
    observer.apply_payload(W1, &OverloadPayload { elu: 0.95, gc: 0.0, adm: 0.0 }, clock.now_ms());
    // …and drain its bucket to empty (a NEW non-emergency call would 503 here).
    while observer.try_consume_for(W1, clock.now_ms()).is_ok() {}
    let s = strategy_with_observer(reg, clock, observer);

    // Sanity: a NEW non-emergency dialog on this worker IS shed (AboveCritical
    // filter → NoTarget, since it is the only worker).
    let new_call = request("INVITE", "call-new@h", None);
    assert!(matches!(
        s.select_for_new_dialog(&new_call, SelectOpts::default()).await,
        Err(SelectError::NoTarget { .. })
    ));

    // But an in-dialog request to the SAME owner is admitted, on the SAME worker,
    // despite AboveCritical + the empty bucket.
    let in_dialog = request("INVITE", "call-est@h", Some("bobtag")); // re-INVITE
    assert_eq!(
        s.select_for_new_dialog(&in_dialog, SelectOpts::default()).await.unwrap(),
        addr(A1),
        "in-dialog must stick to its AboveCritical owner (no band filter, no rate cap)"
    );
    // A second in-dialog request still admits — proves the bucket is bypassed,
    // not merely that one stray token existed.
    let bye = request("BYE", "call-est@h", Some("bobtag"));
    assert_eq!(s.select_for_new_dialog(&bye, SelectOpts::default()).await.unwrap(), addr(A1));
}

#[tokio::test]
async fn decode_stickiness_routes_to_alive_w_pri_regardless_of_elu_band() {
    // The cookie path (`decode_stickiness`) reads the signed `w_pri` and routes
    // back to that owner over the live registry — it never consults the ELU band
    // or the AIMD bucket. So an in-dialog request with a valid cookie pins to its
    // alive owner even when that owner is pegged AboveCritical under starve.
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let reg = two_worker_registry(clock.clone());
    let s = strategy_with_observer(reg.clone(), clock, observer.clone());

    // Mint a cookie for a non-emergency call (names w_pri = the selected owner).
    let invite = request("INVITE", "call-band@h", None);
    let primary = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&primary, &invite).unwrap();
    let primary_id = params.get("w_pri").unwrap().clone();

    // Peg the owning worker AboveCritical — irrelevant to the cookie path.
    observer.apply_payload(&primary_id, &OverloadPayload { elu: 0.99, gc: 0.0, adm: 0.0 }, 0);
    assert_eq!(
        observer.band_for(&primary_id),
        Some(EluBand::AboveCritical),
        "owner is pegged AboveCritical"
    );

    // In-dialog BYE still forwards to the alive primary (band ignored).
    let bye = request("BYE", "call-band@h", Some("bobtag"));
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::Forward { target, .. } => assert_eq!(target, primary),
        other => panic!("expected Forward to the AboveCritical owner, got {other:?}"),
    }
}

#[tokio::test]
async fn no_cookie_in_dialog_request_uses_new_dialog_path_and_can_scatter() {
    // SCATTER RISK: an in-dialog-LOOKING request (To-tag present)
    // that arrives WITHOUT a valid Route cookie does NOT reach `decode_stickiness`
    // — the core only decodes when a signed Route param is present. It would fall
    // through to `select_for_new_dialog`. There, the `is_in_dialog` branch still
    // protects it from the band filter + bucket (it gets ALL alive workers and
    // bypasses AIMD), so it is never SHED — but its target is chosen by
    // rendezvous over the CURRENT alive set, which may differ from the original
    // owner if the cluster resharded. This test documents that contract: no-cookie
    // in-dialog is admitted (never 503'd) but is NOT pinned to the original owner.
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let reg = two_worker_registry(clock.clone());
    // Peg BOTH workers AboveCritical + drain their buckets — the harshest starve.
    for (w, a) in [(W1, A1), (W2, A2)] {
        let _ = a;
        observer.apply_payload(w, &OverloadPayload { elu: 0.99, gc: 0.0, adm: 0.0 }, 0);
        while observer.try_consume_for(w, 0).is_ok() {}
    }
    let s = strategy_with_observer(reg, clock, observer);

    // An in-dialog request with NO cookie (the proxy core would call this when
    // the Route param is absent). It is ADMITTED (in-dialog bypasses both the
    // AboveCritical filter and the empty bucket) and lands on one of the alive
    // workers — never a RateCapExhausted / NoTarget.
    let in_dialog = request("BYE", "call-nocookie@h", Some("bobtag"));
    let target = s.select_for_new_dialog(&in_dialog, SelectOpts::default()).await.unwrap();
    assert!(
        target == addr(A1) || target == addr(A2),
        "no-cookie in-dialog is admitted (rendezvous over alive set), never shed"
    );
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
