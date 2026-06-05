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

/// The full transition the proxy probe observes as a node boots and drains.
#[test]
fn options_reports_not_ready_then_ready_then_draining() {
    let src = FlagSource::new(false, false);
    let r = Readiness::new(src.clone());
    let id_gen = IdGen::seeded(0x57);
    let req = options_probe();

    // NotReady → 503 + Reason text contains "not-ready" (probe → NotReady).
    let resp = build_options_health_response(&r, &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert!(resp.to.tag.is_some(), "503 to out-of-dialog OPTIONS needs a To-tag");
    let reason = reason_of(&resp).expect("NotReady carries a Reason header");
    assert!(
        reason.to_ascii_lowercase().contains("not-ready"),
        "Reason {reason:?} must contain the 'not-ready' token classify_503 keys on"
    );

    // Gate opens → 200 OK (probe → Alive). No Reason header.
    src.set(true, true);
    let resp = build_options_health_response(&r, &id_gen, &req);
    assert_eq!(resp.status, 200);
    assert!(resp.to.tag.is_some(), "200 to out-of-dialog OPTIONS needs a To-tag");
    assert!(reason_of(&resp).is_none());

    // SIGTERM → Draining → 503 + Reason "draining" + Retry-After: 0.
    r.set_draining();
    let resp = build_options_health_response(&r, &id_gen, &req);
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

    assert_eq!(build_options_health_response(&r, &id_gen, &req).status, 200);

    // Peer blip: no longer current/bootstrapped — must NOT revert to 503.
    src.set(false, false);
    let resp = build_options_health_response(&r, &id_gen, &req);
    assert_eq!(resp.status, 200, "latched Ready must not flap to NotReady");
    assert!(reason_of(&resp).is_none());

    // Draining still wins over the latched Ready.
    r.set_draining();
    let resp = build_options_health_response(&r, &id_gen, &req);
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
    let resp = build_options_health_response(&not_ready, &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert_eq!(classify_503(reason_of(&resp).as_deref()), Health::NotReady);

    let draining = Readiness::always_ready();
    draining.set_draining();
    let resp = build_options_health_response(&draining, &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert_eq!(classify_503(reason_of(&resp).as_deref()), Health::Draining);
}

// ── Supervisor-fed transition (real ReplicationSupervisor, paused runtime) ──

use std::net::SocketAddr;
use std::time::Duration;

use repl_net::transport::{ReplicationNetwork, SimulatedReplicationNetwork};
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::{Changelog, FnPeerResolver, PullerConfig, ReplServer, ReplicatingCallStore, ReplicationSupervisor};
use crate::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;

fn fast_config() -> PullerConfig {
    PullerConfig {
        backoff_init_ms: 100,
        backoff_max_ms: 1_000,
        bootstrap_hard_timeout_ms: 2_000,
    }
}

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9700 + n))
}

/// Forward (primary→backup) put options targeting `peer`.
fn fwd(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Forward),
    }
}

fn cref(primary: &str, id: &str) -> String {
    format!("{primary}|{id}|t{id}")
}

async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// Advance ~`ms` in 100 ms chunks, settling between (fake-clock hazard: drive
/// the protocol BETWEEN advances). Mirror of `s6_tests::tick`.
async fn tick(ms: u64) {
    let chunks = ms.div_ceil(100).max(1);
    for _ in 0..chunks {
        settle().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
    }
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
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
        clock.clone(),
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
    let resp = build_options_health_response(&readiness, &id_gen, &req);
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
    let resp = build_options_health_response(&readiness, &id_gen, &req);
    assert_eq!(resp.status, 200);
    assert!(reason_of(&resp).is_none());

    // SIGTERM → 503 draining + Retry-After: 0.
    readiness.set_draining();
    let resp = build_options_health_response(&readiness, &id_gen, &req);
    assert_eq!(resp.status, 503);
    assert!(reason_of(&resp).unwrap().to_ascii_lowercase().contains("draining"));
    assert_eq!(get_header(&resp.headers, "retry-after"), Some("0"));
}
