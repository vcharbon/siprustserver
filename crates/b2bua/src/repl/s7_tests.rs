//! S7 tests: the readiness state machine + the OPTIONS health responder.
//!
//! Two layers:
//! - **Responder path** — drive [`build_options_health_response`] (the exact
//!   function `router::on_event`'s out-of-dialog OPTIONS branch calls) over a
//!   [`Readiness`] whose source flips, asserting the emitted status + `Reason`
//!   header text matches the contract `sip-proxy::health::probe::classify_503`
//!   keys on (`crates/sip-proxy/src/health/probe.rs:175-180`): a 200 → Alive, a
//!   503 whose Reason contains `not-ready` → NotReady, any other 503 → Draining.
//! - **Supervisor-fed transition** — under a paused runtime, a `Readiness` over
//!   a real [`ReplicationSupervisor`] flips NotReady → Ready as its peers
//!   bootstrap + catch up (advance BETWEEN frames per the fake-clock hazards).
//!
//! The latch + Draining-precedence truth table lives in `readiness::tests`; here
//! we pin the *wire* behaviour (status codes + headers) and the supervisor seam.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sip_message::generators::{
    generate_out_of_dialog_request, ContactSpec, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
    SipTransport, ViaSpec,
};
use sip_message::message_helpers::get_header;
use sip_message::SipRequest;
use sip_txn::IdGen;

use super::{Readiness, ReadinessSource};
use crate::overload::OverloadSignal;
use crate::router::build_options_health_response;

/// A flip-able readiness source (stand-in for the supervisor's two gates).
struct FlagSource {
    bootstrapped: AtomicBool,
    current: AtomicBool,
}
impl FlagSource {
    fn new(b: bool, c: bool) -> Arc<Self> {
        Arc::new(Self {
            bootstrapped: AtomicBool::new(b),
            current: AtomicBool::new(c),
        })
    }
    fn set(&self, b: bool, c: bool) {
        self.bootstrapped.store(b, Ordering::SeqCst);
        self.current.store(c, Ordering::SeqCst);
    }
}
impl ReadinessSource for FlagSource {
    fn all_bootstrapped(&self) -> bool {
        self.bootstrapped.load(Ordering::SeqCst)
    }
    fn all_current(&self) -> bool {
        self.current.load(Ordering::SeqCst)
    }
}

/// A bare out-of-dialog OPTIONS keepalive (tagless To), like a proxy probe.
fn options_probe() -> SipRequest {
    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: "sip:b2bua@127.0.0.1:5070".into(),
        call_id: "probe-w0-1234-ab@10.0.0.1".into(),
        from_uri: "sip:probe@10.0.0.1".into(),
        from_tag: "probe".into(),
        to_uri: "sip:b2bua@127.0.0.1:5070".into(),
        to_tag: None,
        cseq: 1,
        via: Some(ViaSpec {
            local_ip: "10.0.0.1".into(),
            local_port: 5060,
            transport: SipTransport::Udp,
            branch: "z9hG4bKprobe".into(),
            custom_params: vec![],
        }),
        contact: Some(ContactSpec {
            user: "probe".into(),
            host: "10.0.0.1".into(),
            port: 5060,
            uri_params: vec![],
        }),
        max_forwards: Some(70),
        body: vec![],
        content_type: None,
        extra_headers: vec![],
    };
    generate_out_of_dialog_request(OutOfDialogMethod::Options, &opts)
}

fn reason_of(resp: &sip_message::SipResponse) -> Option<String> {
    get_header(&resp.headers, "reason").map(|s| s.to_string())
}

/// A zero-state overload signal for the responder calls below. These tests pin
/// the readiness *status* + `Reason` wire contract, not the `X-Overload` header
/// (that is covered in `router::tests` / `overload::tests`); a fresh signal
/// emits the constant `v=1; elu=0.000; gc=0.000; adm=0` on the 200 path.
fn ov() -> OverloadSignal {
    OverloadSignal::live()
}

/// The full transition the proxy probe observes as a node boots and drains.
#[test]
fn options_reports_not_ready_then_ready_then_draining() {
    let src = FlagSource::new(false, false);
    let r = Readiness::new(src.clone());
    let id_gen = IdGen::seeded(0x57);
    let req = options_probe();

    // NotReady → 503 + Reason text contains "not-ready" (probe → NotReady).
    let resp = build_options_health_response(&r, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert!(resp.to.tag.is_some(), "503 to out-of-dialog OPTIONS needs a To-tag");
    let reason = reason_of(&resp).expect("NotReady carries a Reason header");
    assert!(
        reason.to_ascii_lowercase().contains("not-ready"),
        "Reason {reason:?} must contain the 'not-ready' token classify_503 keys on"
    );

    // Gate opens → 200 OK (probe → Alive). No Reason header.
    src.set(true, true);
    let resp = build_options_health_response(&r, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 200);
    assert!(resp.to.tag.is_some(), "200 to out-of-dialog OPTIONS needs a To-tag");
    assert!(reason_of(&resp).is_none());

    // SIGTERM → Draining → 503 + Reason "draining" + Retry-After: 0.
    r.set_draining();
    let resp = build_options_health_response(&r, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert!(resp.to.tag.is_some());
    let reason = reason_of(&resp).expect("Draining carries a Reason header");
    assert!(
        reason.to_ascii_lowercase().contains("draining"),
        "Reason {reason:?} must contain 'draining'"
    );
    assert!(
        !reason.to_ascii_lowercase().contains("not-ready"),
        "draining must NOT key as not-ready in classify_503"
    );
    assert_eq!(get_header(&resp.headers, "retry-after"), Some("0"));
}

/// Once Ready, a transient peer blip (source goes non-current) keeps OPTIONS at
/// 200 (latch); a subsequent SIGTERM still flips to 503 draining.
#[test]
fn options_latches_ready_across_blip_then_drains() {
    let src = FlagSource::new(true, true);
    let r = Readiness::new(src.clone());
    let id_gen = IdGen::seeded(7);
    let req = options_probe();

    assert_eq!(build_options_health_response(&r, &ov(), &id_gen, &req).status, 200);

    // Peer blip: no longer current/bootstrapped — must NOT revert to 503.
    src.set(false, false);
    let resp = build_options_health_response(&r, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 200, "latched Ready must not flap to NotReady");
    assert!(reason_of(&resp).is_none());

    // Draining still wins over the latched Ready.
    r.set_draining();
    let resp = build_options_health_response(&r, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert!(reason_of(&resp).unwrap().to_ascii_lowercase().contains("draining"));
}

/// The exact Reason header text aligns with `sip-proxy`'s `classify_503`
/// contract (probe.rs:175-180 lower-cases + substring-matches `not-ready`;
/// anything else → Draining). We re-implement that match here and assert the
/// emitted headers classify as intended (classify_503 is private cross-crate).
#[test]
fn emitted_reason_aligns_with_proxy_classify_503() {
    // Mirror of sip-proxy::health::probe::classify_503.
    #[derive(Debug, PartialEq)]
    enum Health {
        NotReady,
        Draining,
    }
    fn classify_503(reason: Option<&str>) -> Health {
        match reason {
            Some(r) if r.to_ascii_lowercase().contains("not-ready") => Health::NotReady,
            _ => Health::Draining,
        }
    }

    let id_gen = IdGen::seeded(1);
    let req = options_probe();

    let not_ready = Readiness::new(FlagSource::new(false, false));
    let resp = build_options_health_response(&not_ready, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert_eq!(classify_503(reason_of(&resp).as_deref()), Health::NotReady);

    let draining = Readiness::always_ready();
    draining.set_draining();
    let resp = build_options_health_response(&draining, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert_eq!(classify_503(reason_of(&resp).as_deref()), Health::Draining);
}

/// migration/08: the OPTIONS-200 self-report carries the worker load signal as
/// `X-Overload: v=1; elu=…; gc=…; adm=…` (the schema
/// `sip_proxy::load_observer::parse_x_overload_header` consumes — that real
/// cross-crate parse is exercised end-to-end in `b2bua-harness`), and the `adm`
/// counter the responder publishes tracks
/// `OverloadSignal::increment_non_emergency_admitted`. The 503 paths do NOT
/// carry it (a 503 already removes the node from new-dialog selection).
#[test]
fn options_200_stamps_x_overload_503_does_not() {
    let id_gen = IdGen::seeded(0x08);
    let req = options_probe();

    // Ready → 200 with an X-Overload header in the exact zero-state v=1 schema.
    let overload = OverloadSignal::live();
    let ready = Readiness::always_ready();
    let resp = build_options_health_response(&ready, &overload, &id_gen, &req);
    assert_eq!(resp.status, 200);
    let xo = get_header(&resp.headers, "x-overload")
        .expect("OPTIONS 200 must advertise the worker load signal");
    assert_eq!(xo, "v=1; elu=0.000; gc=0.000; adm=0");

    // Advance the admit counter; the next 200's header reflects it as adm=2.
    overload.increment_non_emergency_admitted();
    overload.increment_non_emergency_admitted();
    let resp = build_options_health_response(&ready, &overload, &id_gen, &req);
    let xo = get_header(&resp.headers, "x-overload").unwrap();
    assert_eq!(
        xo, "v=1; elu=0.000; gc=0.000; adm=2",
        "adm must track increment_non_emergency_admitted"
    );

    // NotReady (503) and Draining (503) carry NO X-Overload.
    let not_ready = Readiness::new(FlagSource::new(false, false));
    let resp = build_options_health_response(&not_ready, &overload, &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert!(
        get_header(&resp.headers, "x-overload").is_none(),
        "a 503 (not-ready) self-report must not carry the band signal"
    );

    let draining = Readiness::always_ready();
    draining.set_draining();
    let resp = build_options_health_response(&draining, &overload, &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert!(
        get_header(&resp.headers, "x-overload").is_none(),
        "a 503 (draining) self-report must not carry the band signal"
    );
}

// ── Supervisor-fed transition (real ReplicationSupervisor, paused runtime) ──

use std::net::SocketAddr;

use repl_net::transport::{ReplicationNetwork, SimulatedReplicationNetwork};
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::test_support::{cref, fast_config, fwd, tick};
use super::{Changelog, FnPeerResolver, ReplServer, ReplicatingCallStore, ReplicationSupervisor};
use crate::store::{CallStore, PartitionRole};

const PRI: PartitionRole = PartitionRole::Primary;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9700 + n))
}

/// A `Readiness` over a real [`ReplicationSupervisor`] reports NotReady until
/// its sole peer bootstraps + catches up, then latches Ready — driving the
/// OPTIONS responder 503(not-ready) → 200 → 503(draining).
#[tokio::test(start_paused = true)]
async fn supervisor_readiness_flips_not_ready_to_ready_to_draining() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));

    // Peer B serves a changelog; it already holds A's call in bak:A so our
    // node A can bootstrap it as pri:A and the tail keeps us current.
    let b_changelog = Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000);
    let b_store = ReplicatingCallStore::with_changelog(b_changelog.clone(), clock.clone());
    let b_addr = addr(2);
    let b_listener = net.listen(b_addr).await.unwrap();
    let b_server = ReplServer::new("B", b_changelog, Arc::new(b_store.clone()));
    tokio::spawn(b_server.run(b_listener));

    // Seed B with A's call in bak:A (what A will reclaim on bootstrap).
    let c = cref("A", "0");
    b_store
        .put_call(PartitionRole::Backup, "A", &c, b"body0".to_vec(), &[], 0, 1, 0, &fwd("A"))
        .await
        .unwrap();

    // Our node A: empty store, supervisor pulling B.
    let a_store = ReplicatingCallStore::new(2, clock.clone());
    let resolve = Arc::new(FnPeerResolver(move |peer: &Peer| {
        assert_eq!(peer.ordinal, "B");
        b_addr
    }));
    let a_sup = ReplicationSupervisor::with_config(
        "A",
        net.clone(),
        a_store.clone(),
        resolve,
        fast_config(),
    );

    let readiness = Readiness::new(Arc::new(a_sup.clone()));
    let id_gen = IdGen::seeded(0xBEEF);
    let req = options_probe();

    // Before start/catch-up: no peers known yet OR not current → NotReady. We
    // start the supervisor first so a peer exists and is genuinely not-current.
    a_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));
    let resp = build_options_health_response(&readiness, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 503, "before catch-up: NotReady");
    assert!(reason_of(&resp).unwrap().to_ascii_lowercase().contains("not-ready"));

    // Drive bootstrap + tail to current.
    tick(300).await;
    assert!(a_sup.all_bootstrapped(), "A bootstraps from B");
    assert!(a_sup.all_current(), "A's tail catches up");
    // Sanity: A reclaimed the call as pri:A.
    assert_eq!(
        a_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(b"body0".as_ref()),
    );

    // Gate now open → 200 OK; readiness latches.
    let resp = build_options_health_response(&readiness, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 200);
    assert!(reason_of(&resp).is_none());

    // SIGTERM → 503 draining + Retry-After: 0.
    readiness.set_draining();
    let resp = build_options_health_response(&readiness, &ov(), &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert!(reason_of(&resp).unwrap().to_ascii_lowercase().contains("draining"));
    assert_eq!(get_header(&resp.headers, "retry-after"), Some("0"));
}

/// Cold double-restart readiness deadlock regression (handoff
/// `repl-coldstart-readiness-deadlock`): a peer that is seen **once** at a stale
/// address, never connects, then **leaves** the desired membership (both pods
/// NotReady → `publishNotReadyAddresses:false` empties the EndpointSlice) is
/// parked with `{current:false, bootstrap_complete:false, ever_connected:false}`
/// and retained in the `peers` map. Before the fix `all_current`/
/// `all_bootstrapped` iterated **every** retained entry, so that parked-departed
/// peer pinned the node NotReady forever — no puller was left to fire the
/// bootstrap hard timer. The node must still reach Ready (the peer is no longer
/// desired; readiness filters to the desired membership, like the backup gate).
#[tokio::test(start_paused = true)]
async fn departed_unreachable_peer_does_not_wedge_readiness_not_ready() {
    let clock = Clock::test_at(0);
    // No B server is ever spawned: every connect to B's (stale) addr fails, so
    // B's Reclaim flow never connects (ever_connected stays false) and — parked
    // before the 2 s hard timer — never goes bootstrap-complete.
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b_addr = addr(2);
    let resolve = Arc::new(FnPeerResolver(move |peer: &Peer| {
        assert_eq!(peer.ordinal, "B");
        b_addr
    }));

    let a_store = ReplicatingCallStore::new(2, clock.clone());
    let a_sup = ReplicationSupervisor::with_config(
        "A",
        net.clone(),
        a_store.clone(),
        resolve,
        fast_config(),
    );

    // Boot seeing the peer once (its pre-restart, now-stale identity).
    let membership = Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    ));
    a_sup.start(membership.clone());

    // Let the Reclaim puller spawn + fail its first connect. Stay well under the
    // 2 s bootstrap hard timer so it is genuinely still pending (not yet
    // best-effort complete) — the exact pre-condition the bug needs.
    tick(100).await;
    assert!(!a_sup.all_current(), "peer B still pending → NotReady (sanity)");
    assert!(a_sup.is_running("B"), "B's Reclaim puller is running while desired");

    // EndpointSlice empties: B leaves the desired membership before its hard
    // timer fires → reconcile parks B (entry retained, flags all false).
    membership.remove("B");
    tick(100).await; // let the watch-driven reconcile park B
    assert!(!a_sup.is_running("B"), "departed B is parked (puller cancelled)");

    // Still BEFORE the 2 s hard timer (≈200 ms elapsed): B never went
    // bootstrap-complete. The node must nonetheless be Ready — B is no longer
    // desired, so its parked entry must not pin readiness.
    assert!(
        a_sup.all_bootstrapped(),
        "departed parked peer must not block all_bootstrapped"
    );
    assert!(
        a_sup.all_current(),
        "departed parked peer must not block all_current (cold-start deadlock)"
    );

    // End to end through the readiness latch + OPTIONS responder: 200 OK.
    let readiness = Readiness::new(Arc::new(a_sup.clone()));
    let id_gen = IdGen::seeded(0xC0FFEE);
    let resp = build_options_health_response(&readiness, &ov(), &id_gen, &options_probe());
    assert_eq!(resp.status, 200, "peerless-after-departure node serves Ready");
    assert!(reason_of(&resp).is_none());
}
