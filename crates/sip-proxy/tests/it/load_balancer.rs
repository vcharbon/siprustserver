//! The load balancer's pure logic: cookie encode/decode round-trip, HMAC
//! tampering, the liveness routing matrix and the acceptance of new calls
//! (ADR-0050), unresolvable-id fallback, resharding stickiness, and
//! initial-health gating. No proxy SUT — these exercise the
//! `LoadBalancerStrategy` directly.

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

fn request(method: &str, call_id: &str, to_tag: Option<&str>) -> SipMessage {
    request_with_rph(method, call_id, to_tag, None)
}

/// As [`request`], optionally stamping a `Resource-Priority` header (the on-wire
/// emergency signal `is_emergency_invite` detects, `Resource-Priority:
/// esnet.0`).
/// Its Via branch is the Call-ID with `@` replaced, a branch being a token
/// (RFC 3261 §25.1).
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
    let branch = call_id.replace('@', ".");
    let raw = format!(
        "{method} sip:bob@10.0.0.3:5070 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{branch}\r\n\
From: <sip:alice@a>;tag=fromtag\r\n\
To: {to}\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 {method}\r\n\
Max-Forwards: 70\r\n\
{rph_line}\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::new().parse(raw.as_bytes()).unwrap()
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

/// The proxy stamps the v3 cookie wire format (v=3, e=0, w_pri/w_bak/kid/sig
/// present), and a cookie whose signature is altered is refused with a 403.
/// The altered signature keeps its length, so the refusal is the MAC
/// mismatch, not a malformed MAC.
#[tokio::test]
async fn hmac_tampering_rejected() {
    let reg = two_worker_registry(Clock::test_at(0));
    let s = strategy(reg, Clock::test_at(0));
    let invite = request("INVITE", "call-tamper@h", None);
    let target = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let mut params = s.encode_stickiness(&target, &invite).unwrap();

    assert_eq!(params.get("v").unwrap(), "3");
    assert_eq!(params.get("e").unwrap(), "0", "non-emergency INVITE ⇒ e=0");
    assert!(!params.get("w_pri").unwrap().is_empty(), "w_pri present");
    assert!(params.contains_key("w_bak"), "w_bak present (may be empty)");
    assert!(!params.get("kid").unwrap().is_empty(), "kid present");
    assert!(!params.get("sig").unwrap().is_empty(), "sig present");

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

/// The v3 `e=1` emergency flag survives an encode → decode round-trip: an
/// emergency dialog encodes `e=1`, and a decoded `e=1` is an emergency dialog.
#[tokio::test]
async fn emergency_cookie_round_trips_e1_and_is_emergency() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg, clock);

    let invite = emergency_invite("call-em-rt@h");
    let target = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&target, &invite).unwrap();
    assert_eq!(params.get("v").unwrap(), "3");
    assert_eq!(params.get("e").unwrap(), "1", "emergency INVITE ⇒ e=1");

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

    reg.set_health(&primary_id, WorkerHealth::Dead);
    let backup_addr = reg.resolve(&backup_id).unwrap().address;
    let bye = request("BYE", "call-fb@h", Some("bobtag"));
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::ForwardBackup { target, .. } => assert_eq!(target, backup_addr),
        other => panic!("expected ForwardBackup, got {other:?}"),
    }
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

/// Above the critical band a non-emergency new dialog finds no target.
/// Emergency bypasses the band filter on two independent paths, one assertion
/// each: an on-wire `Resource-Priority: esnet.0` INVITE drives
/// `is_emergency_invite` on the select path directly, and the explicit
/// `emergency_override` opts shortcut (used by the dispatcher-marked in-dialog
/// path) bypasses the filter on a non-RPH INVITE.
#[tokio::test]
async fn above_critical_band_filtered_for_non_emergency_only() {
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let reg =
        SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W1, addr(A1))], clock.clone());
    observer.apply_payload(W1, &OverloadPayload { elu: 0.95, gc: 0.0, adm: 0.0 }, clock.now_ms());
    let s = strategy_with_observer(reg, clock, observer);

    let invite = request("INVITE", "call-ov@h", None);
    assert!(matches!(
        s.select_for_new_dialog(&invite, SelectOpts::default()).await,
        Err(SelectError::NoTarget { .. })
    ));
    let rph_invite = emergency_invite("call-ov-rph@h");
    assert_eq!(
        s.select_for_new_dialog(&rph_invite, SelectOpts::default()).await.unwrap(),
        addr(A1)
    );
    let emergency = SelectOpts { emergency_override: true, ..SelectOpts::default() };
    assert_eq!(s.select_for_new_dialog(&invite, emergency).await.unwrap(), addr(A1));
}

/// The per-worker AIMD bucket gates admission: a non-emergency new dialog
/// whose rendezvous winner has a drained bucket is refused with
/// `RateCapExhausted`, and a re-INVITE or an emergency dialog on the same
/// worker bypasses the bucket. The paused clock keeps the bucket from
/// refilling between the drain and the selection.
#[tokio::test(start_paused = true)]
async fn rate_cap_exhausted_when_winners_bucket_is_empty() {
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let reg =
        SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W1, addr(A1))], clock.clone());
    observer.apply_payload(W1, &OverloadPayload { elu: 0.1, gc: 0.0, adm: 0.0 }, clock.now_ms());
    while observer.try_consume_for(W1, clock.now_ms()).is_ok() {}
    let s = strategy_with_observer(reg, clock, observer);

    let invite = request("INVITE", "call-rc@h", None);
    match s.select_for_new_dialog(&invite, SelectOpts::default()).await {
        Err(SelectError::RateCapExhausted { worker_id, retry_after_sec }) => {
            assert_eq!(worker_id, W1);
            assert!(retry_after_sec >= 1, "Retry-After must be a usable, non-zero value");
        }
        other => panic!("expected RateCapExhausted, got {other:?}"),
    }

    let reinvite = request("INVITE", "call-rc@h", Some("bobtag"));
    assert_eq!(s.select_for_new_dialog(&reinvite, SelectOpts::default()).await.unwrap(), addr(A1));

    let emergency = request("INVITE", "call-rc-em@h", None);
    let opts = SelectOpts { emergency_override: true, ..SelectOpts::default() };
    assert_eq!(s.select_for_new_dialog(&emergency, opts).await.unwrap(), addr(A1));
}

/// A request naming a dialog no verified cookie backs (`admit_as_new`) is a
/// new call: the empty bucket an in-dialog request bypasses refuses it.
#[tokio::test(start_paused = true)]
async fn a_dialog_admitted_as_new_meets_the_empty_bucket() {
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

/// A worker no load payload was read from has no bucket and is admitted: no
/// rate cap applies before its first OPTIONS answer.
#[tokio::test]
async fn unobserved_worker_is_admitted_without_a_bucket() {
    let clock = Clock::test_at(0);
    let reg = two_worker_registry(clock.clone());
    let s = strategy(reg, clock);
    let invite = request("INVITE", "call-boot@h", None);
    let target = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    assert!(target == addr(A1) || target == addr(A2));
}

/// Under default `SelectOpts`, an in-dialog request (To-tag present) whose
/// owner is both above the critical band and out of tokens still selects that
/// owner: the band filter and the bucket shed new calls only.
#[tokio::test(start_paused = true)]
async fn in_dialog_sticks_to_owner_in_above_critical_and_bypasses_empty_bucket() {
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let reg =
        SimulatedWorkerRegistry::with_clock(vec![WorkerEntry::alive(W1, addr(A1))], clock.clone());
    observer.apply_payload(W1, &OverloadPayload { elu: 0.95, gc: 0.0, adm: 0.0 }, clock.now_ms());
    while observer.try_consume_for(W1, clock.now_ms()).is_ok() {}
    let s = strategy_with_observer(reg, clock, observer);

    let new_call = request("INVITE", "call-new@h", None);
    assert!(matches!(
        s.select_for_new_dialog(&new_call, SelectOpts::default()).await,
        Err(SelectError::NoTarget { .. })
    ));

    let in_dialog = request("INVITE", "call-est@h", Some("bobtag"));
    assert_eq!(
        s.select_for_new_dialog(&in_dialog, SelectOpts::default()).await.unwrap(),
        addr(A1),
        "in-dialog must stick to its AboveCritical owner (no band filter, no rate cap)"
    );
    let bye = request("BYE", "call-est@h", Some("bobtag"));
    assert_eq!(s.select_for_new_dialog(&bye, SelectOpts::default()).await.unwrap(), addr(A1));
}

/// The cookie path reads the signed `w_pri` over the live registry, never the
/// ELU band or the bucket: a valid cookie reaches its alive owner above the
/// critical band.
#[tokio::test]
async fn decode_stickiness_routes_to_alive_w_pri_regardless_of_elu_band() {
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let reg = two_worker_registry(clock.clone());
    let s = strategy_with_observer(reg.clone(), clock, observer.clone());

    let invite = request("INVITE", "call-band@h", None);
    let primary = s.select_for_new_dialog(&invite, SelectOpts::default()).await.unwrap();
    let params = s.encode_stickiness(&primary, &invite).unwrap();
    let primary_id = params.get("w_pri").unwrap().clone();

    observer.apply_payload(&primary_id, &OverloadPayload { elu: 0.99, gc: 0.0, adm: 0.0 }, 0);
    assert_eq!(
        observer.band_for(&primary_id),
        Some(EluBand::AboveCritical),
        "owner is pegged AboveCritical"
    );

    let bye = request("BYE", "call-band@h", Some("bobtag"));
    match s.decode_stickiness(&params, &bye).await {
        DecodeResult::Forward { target, .. } => assert_eq!(target, primary),
        other => panic!("expected Forward to the AboveCritical owner, got {other:?}"),
    }
}

/// Under default `SelectOpts`, a request with a To-tag and no cookie (here a
/// BYE) passes the band filter and the bucket, so it is never shed, and lands
/// on the rendezvous winner over the current alive set, which a reshard may
/// have moved off the dialog's owner.
#[tokio::test]
async fn no_cookie_in_dialog_request_uses_new_dialog_path_and_can_scatter() {
    let clock = Clock::test_at(0);
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let reg = two_worker_registry(clock.clone());
    for (w, a) in [(W1, A1), (W2, A2)] {
        let _ = a;
        observer.apply_payload(w, &OverloadPayload { elu: 0.99, gc: 0.0, adm: 0.0 }, 0);
        while observer.try_consume_for(w, 0).is_ok() {}
    }
    let s = strategy_with_observer(reg, clock, observer);

    let in_dialog = request("BYE", "call-nocookie@h", Some("bobtag"));
    let target = s.select_for_new_dialog(&in_dialog, SelectOpts::default()).await.unwrap();
    assert!(
        target == addr(A1) || target == addr(A2),
        "no-cookie in-dialog is admitted (rendezvous over alive set), never shed"
    );
}

#[tokio::test]
async fn resharding_keeps_in_dialog_on_cookie_primary() {
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
