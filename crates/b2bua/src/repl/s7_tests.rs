//! S7 tests: the readiness state machine + the OPTIONS health responder.
//!
//! Two layers:
//! - **Responder path** — drive [`build_options_health_response`] (the exact
//!   function `router::on_event`'s out-of-dialog OPTIONS branch calls) over a
//!   [`Readiness`] whose source flips, asserting the emitted status and the
//!   `X-Readiness` / `X-Incarnation` / `X-Overload` headers the proxy's health
//!   probe reads (ADR-0050 §2): `200` while the node serves dialogs, with its
//!   acceptance of new calls; `503` while it is not ready.
//! - **Supervisor-fed transition** — under a paused runtime, a `Readiness` over
//!   a real [`ReplicationSupervisor`] flips NotReady → Ready as its peers
//!   bootstrap + catch up (advance BETWEEN frames per the fake-clock hazards).
//!
//! The latch + Draining-precedence truth table lives in `readiness::tests`; here
//! we pin the *wire* behaviour (status codes + headers) and the supervisor seam.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sip_message::generators::{
    generate_out_of_dialog_request, CapabilitySet, GenerateOutOfDialogRequestOpts,
    OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::worker_report::{self, Readiness as WorkerReadiness};
use sip_message::HeaderName;
use sip_message::SipRequest;
use sip_message::SipStr;
use sip_txn::{IdGen, StatelessTagger};

use super::{PeerLink, Readiness, ReadinessSource};
use crate::overload::OverloadSignal;
use crate::router::build_options_health_response as options_response;

/// The responder's answer to `req`, which every probe here draws.
fn build_options_health_response(
    r: &Readiness,
    overload: &OverloadSignal,
    tagger: &StatelessTagger,
    req: &SipRequest,
    capabilities: &CapabilitySet,
) -> sip_message::SipResponse {
    options_response(r, overload, tagger, req, capabilities).expect("an OPTIONS answer")
}

/// A flip-able readiness source (stand-in for the supervisor's two gates).
struct FlagSource {
    bootstrapped: AtomicBool,
    current: AtomicBool,
}
impl FlagSource {
    fn new(b: bool, c: bool) -> Arc<Self> {
        Arc::new(Self { bootstrapped: AtomicBool::new(b), current: AtomicBool::new(c) })
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

/// The URI a fixture names as text.
fn uri_of(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

fn options_probe() -> SipRequest {
    options_probe_on("z9hG4bKprobe")
}

/// [`options_probe`] on the top-`Via` branch `branch`.
fn options_probe_on(branch: &str) -> SipRequest {
    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: Some(uri_of("sip:b2bua@127.0.0.1:5070")),
        call_id: "probe-w0-1234-ab@10.0.0.1".into(),
        from: Some(
            header::From::from_uri(uri_of("sip:probe@10.0.0.1"))
                .with_tag(SipStr::from_static("probe")),
        ),
        to: Some(header::To::from_uri(uri_of("sip:b2bua@127.0.0.1:5070"))),
        cseq: 1,
        via: Some(Via::udp("10.0.0.1", 5060).with_branch(SipStr::owned(branch))),
        contact: Some(header::Contact::from_uri(
            Uri::sip_user("probe", "10.0.0.1").with_port(5060),
        )),
        max_forwards: Some(70),
        body: vec![],
        content_type: None,
        extra_headers: vec![],
    };
    generate_out_of_dialog_request(OutOfDialogMethod::Options, &opts).expect("an OPTIONS")
}

/// The acceptance of new calls a self-report states, if any.
fn readiness_of(resp: &sip_message::SipResponse) -> Option<WorkerReadiness> {
    worker_report::readiness(resp)
}

/// The `X-Overload` band signal a self-report carries, if any.
fn x_overload(resp: &sip_message::SipResponse) -> Option<&str> {
    resp.raw(HeaderName::from("X-Overload")).next()
}

/// A zero-state overload signal for the responder calls below; a fresh signal
/// emits the constant `v=1; elu=0.000; gc=0.000; adm=0` on the 200 path.
fn ov() -> OverloadSignal {
    OverloadSignal::live()
}

fn refusing(cause: &str) -> Option<WorkerReadiness> {
    Some(WorkerReadiness::refuse(cause))
}

/// No transaction holds the answer, so every copy of one OPTIONS draws the
/// same To-tag and another OPTIONS draws its own (RFC 3261 §8.2.7).
#[test]
fn options_answer_to_tag_is_derived_from_the_request() {
    let r = Readiness::always_ready();
    let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(0x8_2_7));
    let tag = |req: &SipRequest| {
        let resp =
            build_options_health_response(&r, &ov(), &tagger, req, &CapabilitySet::default());
        resp.to().tag().expect("a To-tag").to_string()
    };

    assert_eq!(tag(&options_probe()), tag(&options_probe()), "a copy draws the same tag");
    assert_ne!(
        tag(&options_probe()),
        tag(&options_probe_on("z9hG4bKnext")),
        "another draws its own"
    );
}

/// The full transition the proxy probe observes as a node boots and drains:
/// `503` with nothing but its incarnation, then `200` accepting, then `200`
/// refusing with `cause=draining`, each stating the same incarnation.
#[test]
fn options_reports_not_ready_then_ready_then_draining() {
    let src = FlagSource::new(false, false);
    let r = Readiness::new(src.clone()).with_incarnation(1_792_155_573_698);
    let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(0x57));
    let req = options_probe();

    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 503);
    assert!(resp.to().tag().is_some(), "503 to out-of-dialog OPTIONS needs a To-tag");
    assert_eq!(readiness_of(&resp), None, "a node serving nothing states no acceptance");
    assert_eq!(x_overload(&resp), None, "nor a load payload");
    assert_eq!(worker_report::incarnation(&resp), Some(1_792_155_573_698));

    src.set(true, true);
    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 200);
    assert!(resp.to().tag().is_some(), "200 to out-of-dialog OPTIONS needs a To-tag");
    assert_eq!(readiness_of(&resp), Some(WorkerReadiness::accept()));
    assert_eq!(worker_report::incarnation(&resp), Some(1_792_155_573_698));

    r.set_draining();
    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 200, "a draining node still serves its dialogs");
    assert_eq!(readiness_of(&resp), refusing("draining"));
    assert!(x_overload(&resp).is_some(), "with its load payload");
    assert_eq!(worker_report::incarnation(&resp), Some(1_792_155_573_698));
}

/// Once Ready, a transient peer blip (source goes non-current) keeps OPTIONS at
/// 200 accepting (latch); a subsequent SIGTERM refuses new calls.
#[test]
fn options_latches_ready_across_blip_then_drains() {
    let src = FlagSource::new(true, true);
    let r = Readiness::new(src.clone());
    let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(7));
    let req = options_probe();

    assert_eq!(
        build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default()).status(),
        200
    );

    // Peer blip: no longer current/bootstrapped — must NOT revert to 503.
    src.set(false, false);
    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 200, "latched Ready must not flap to NotReady");
    assert_eq!(readiness_of(&resp), Some(WorkerReadiness::accept()));

    // Draining still wins over the latched Ready.
    r.set_draining();
    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 200);
    assert_eq!(readiness_of(&resp), refusing("draining"));
}

/// A gating backend's open breaker refuses new calls on a `200` that keeps
/// the load payload; the breaker closing accepts again.
#[test]
fn an_open_backend_breaker_refuses_new_calls_on_a_200() {
    let r = Readiness::always_ready();
    let (breaker, open) = tokio::sync::watch::channel(true);
    r.gate_options_on(open);
    let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(9));
    let req = options_probe();

    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 200);
    assert_eq!(readiness_of(&resp), refusing("backend"));
    assert!(x_overload(&resp).is_some(), "a refusing node keeps its load payload");

    breaker.send_replace(false);
    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(readiness_of(&resp), Some(WorkerReadiness::accept()));
}

/// Every OPTIONS-200 self-report carries the worker load signal as
/// `X-Overload: v=1; elu=…; gc=…; adm=…` (the schema
/// `sip_proxy::load_observer::parse_x_overload_header` consumes), a refusing
/// node's included, and the `adm` counter tracks
/// `OverloadSignal::increment_non_emergency_admitted`. The 503 of a node that
/// serves nothing yet carries none.
#[test]
fn options_200_stamps_x_overload_503_does_not() {
    let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(0x08));
    let req = options_probe();

    // Ready → 200 with an X-Overload header in the exact zero-state v=1 schema.
    let overload = OverloadSignal::live();
    let ready = Readiness::always_ready();
    let resp =
        build_options_health_response(&ready, &overload, &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 200);
    let xo = x_overload(&resp).expect("OPTIONS 200 must advertise the worker load signal");
    assert_eq!(xo, "v=1; elu=0.000; gc=0.000; adm=0");

    // Advance the admit counter; the next 200's header reflects it as adm=2.
    overload.increment_non_emergency_admitted();
    overload.increment_non_emergency_admitted();
    let resp =
        build_options_health_response(&ready, &overload, &tagger, &req, &CapabilitySet::default());
    let xo = x_overload(&resp).unwrap();
    assert_eq!(
        xo, "v=1; elu=0.000; gc=0.000; adm=2",
        "adm must track increment_non_emergency_admitted"
    );

    // NotReady (503) carries NO X-Overload; Draining (200) does.
    let not_ready = Readiness::new(FlagSource::new(false, false));
    let resp = build_options_health_response(
        &not_ready,
        &overload,
        &tagger,
        &req,
        &CapabilitySet::default(),
    );
    assert_eq!(resp.status(), 503);
    assert!(
        x_overload(&resp).is_none(),
        "a 503 (not-ready) self-report must not carry the band signal"
    );

    let draining = Readiness::always_ready();
    draining.set_draining();
    let resp = build_options_health_response(
        &draining,
        &overload,
        &tagger,
        &req,
        &CapabilitySet::default(),
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(
        x_overload(&resp),
        Some("v=1; elu=0.000; gc=0.000; adm=2"),
        "a draining node keeps stating its load"
    );
}

// ── Supervisor-fed transition (real ReplicationSupervisor, paused runtime) ──

use std::net::SocketAddr;

use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::test_support::{cref, fast_config, fwd, tick};
use super::{
    Changelog, FnPeerResolver, ReplServer, ReplicatingCallStore, ReplicationSupervisor,
    SelfEndpoint, WithdrawalCondition,
};
use crate::store::{CallStore, PartitionRole};

const PRI: PartitionRole = PartitionRole::Primary;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9700 + n))
}

/// A `Readiness` over a real [`ReplicationSupervisor`] reports NotReady until
/// its sole peer bootstraps + catches up, then latches Ready — driving the
/// OPTIONS responder 503 → 200 accepting → 200 refusing (draining).
#[tokio::test(start_paused = true)]
async fn supervisor_readiness_flips_not_ready_to_ready_to_draining() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));

    // Peer B serves a changelog; it already holds A's call in bak:A so our
    // node A can bootstrap it as pri:A and the tail keeps us current.
    let b_changelog = Changelog::new(1, clock.clone());
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
    let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(0xBEEF));
    let req = options_probe();

    // Before start/catch-up: no peers known yet OR not current → NotReady. We
    // start the supervisor first so a peer exists and is genuinely not-current.
    a_sup
        .start(Arc::new(SimulatedMembership::with_clock(vec![Peer::new("B", "B")], clock.clone())));
    let resp =
        build_options_health_response(&readiness, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 503, "before catch-up: NotReady");
    assert_eq!(readiness_of(&resp), None);

    // Drive bootstrap + tail to current.
    tick(300).await;
    assert!(a_sup.all_bootstrapped(), "A bootstraps from B");
    assert!(a_sup.all_current(), "A's tail catches up");
    // Sanity: A reclaimed the call as pri:A.
    assert_eq!(a_store.get_call(PRI, "A", &c).await.unwrap().as_deref(), Some(b"body0".as_ref()),);

    // Gate now open → 200 OK; readiness latches.
    let resp =
        build_options_health_response(&readiness, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 200);
    assert_eq!(readiness_of(&resp), Some(WorkerReadiness::accept()));

    // SIGTERM → still serving, refusing new calls.
    readiness.set_draining();
    let resp =
        build_options_health_response(&readiness, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(resp.status(), 200);
    assert_eq!(readiness_of(&resp), refusing("draining"));
}

/// A worker whose own endpoint leaves the membership it pulls from observes its
/// withdrawal from routing (ADR-0031 D6): the supervisor latches `is_withdrawn`,
/// wakes its watch, raises the `withdrawn_running` gauge, and the readiness
/// latch it feeds refuses new calls (`cause=draining`) without any SIGTERM. Boot-time
/// absence is not a withdrawal: the latch waits for the first routable sighting.
#[tokio::test(start_paused = true)]
async fn own_endpoint_withdrawn_latches_without_sigterm() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a_store = ReplicatingCallStore::new(1, clock.clone());
    let resolve = Arc::new(FnPeerResolver(|_: &Peer| addr(9)));
    let a_sup = ReplicationSupervisor::with_config(
        "A",
        net.clone(),
        a_store.clone(),
        resolve,
        fast_config(),
    );
    let readiness = Readiness::new(Arc::new(a_sup.clone()));
    let mut own = a_sup.self_endpoint();

    // Not yet published: the informer shows the pool without this worker.
    let membership = SimulatedMembership::with_clock(vec![], clock.clone());
    a_sup.start(Arc::new(membership.clone()));
    tick(100).await;
    assert!(!a_sup.is_withdrawn(), "absent before publication is not a withdrawal");
    assert_eq!(*own.borrow_and_update(), SelfEndpoint::Unobserved);

    // Published: seen routable once.
    membership.add(Peer::new("A", "A"));
    tick(100).await;
    assert!(!a_sup.is_withdrawn());
    assert_eq!(*own.borrow_and_update(), SelfEndpoint::Routable);
    assert!(!a_sup.metrics().withdrawn_running());

    // Withdrawn from routing, no SIGTERM anywhere.
    membership.remove("A");
    tick(100).await;
    assert!(a_sup.is_withdrawn(), "the worker observed its own withdrawal");
    assert_eq!(*own.borrow_and_update(), SelfEndpoint::Withdrawn(WithdrawalCondition::Absent));
    assert!(a_sup.metrics().withdrawn_running(), "the withdrawn-but-running gauge is raised");

    // The core turns the watch into the drain latch; the responder then reports
    // draining, as it does after SIGTERM.
    readiness.set_withdrawn();
    assert!(readiness.is_draining());
    let resp = build_options_health_response(
        &readiness,
        &ov(),
        &StatelessTagger::from_id_gen(&IdGen::seeded(1)),
        &options_probe(),
        &CapabilitySet::default(),
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(readiness_of(&resp), refusing("draining"));

    // Sticky: a re-publication does not un-withdraw.
    membership.add(Peer::new("A", "A"));
    tick(100).await;
    assert!(a_sup.is_withdrawn());
}

/// A static membership never shows a worker its own endpoint, so a worker on
/// one never observes a withdrawal, whether or not the list names it.
#[tokio::test(start_paused = true)]
async fn static_membership_never_withdraws_the_worker() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    for peers in [vec![Peer::new("B", "B")], vec![Peer::new("A", "A"), Peer::new("B", "B")]] {
        let a_store = ReplicatingCallStore::new(1, clock.clone());
        let resolve = Arc::new(FnPeerResolver(|_: &Peer| addr(9)));
        let a_sup = ReplicationSupervisor::with_config(
            "A",
            net.clone(),
            a_store.clone(),
            resolve,
            fast_config(),
        );
        a_sup.start(Arc::new(topology::StaticMembership::from_peers(peers)));
        tick(200).await;
        assert!(!a_sup.is_withdrawn());
        assert_eq!(*a_sup.self_endpoint().borrow(), SelfEndpoint::Unobserved);
        assert!(!a_sup.metrics().withdrawn_running());
        a_sup.shutdown();
    }
}

/// Cold double-restart readiness deadlock: a peer that is seen **once** at a
/// stale address, never connects, then **leaves** the desired membership (both
/// pods NotReady → `publishNotReadyAddresses:false` empties the EndpointSlice) is
/// parked with `{current:false, bootstrap_complete:false, ever_connected:false}`
/// and retained in the `peers` map. Were `all_current`/`all_bootstrapped` to
/// iterate **every** retained entry, that parked-departed peer would pin the node
/// NotReady forever — no puller is left to fire the bootstrap hard timer. The
/// node must still reach Ready (the peer is no longer desired; readiness filters
/// to the desired membership, like the backup gate).
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
    let membership =
        Arc::new(SimulatedMembership::with_clock(vec![Peer::new("B", "B")], clock.clone()));
    a_sup.start(membership.clone());

    // Let the Reclaim puller spawn + fail its first connect. Stay well under the
    // 2 s bootstrap hard timer so it is genuinely still pending (not yet
    // best-effort complete) — the exact pre-condition of the deadlock.
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
    assert!(a_sup.all_bootstrapped(), "departed parked peer must not block all_bootstrapped");
    assert!(
        a_sup.all_current(),
        "departed parked peer must not block all_current (cold-start deadlock)"
    );

    // End to end through the readiness latch + OPTIONS responder: 200 OK.
    let readiness = Readiness::new(Arc::new(a_sup.clone()));
    let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(0xC0FFEE));
    let resp = build_options_health_response(
        &readiness,
        &ov(),
        &tagger,
        &options_probe(),
        &CapabilitySet::default(),
    );
    assert_eq!(resp.status(), 200, "peerless-after-departure node serves Ready");
    assert_eq!(readiness_of(&resp), Some(WorkerReadiness::accept()));
}

/// The node capability advertisement on the OPTIONS health 200 (RFC 3261
/// §11.2): the node-scoped declaration, and the stack set when none is made.
#[test]
fn options_200_advertises_the_node_capability_set() {
    let r = Readiness::new(FlagSource::new(true, true));
    let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(0xA11));
    let req = options_probe();
    let value = |resp: &sip_message::SipResponse, name: HeaderName| {
        resp.raw_text(name).next().map(|v| v.as_str().to_string())
    };

    // Undeclared → the stack set, byte for byte.
    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &CapabilitySet::default());
    assert_eq!(
        value(&resp, HeaderName::Allow).as_deref(),
        Some(sip_message::generators::B2BUA_ALLOW)
    );
    assert_eq!(
        value(&resp, HeaderName::Supported).as_deref(),
        Some(sip_message::generators::B2BUA_SUPPORTED)
    );

    // Declared → exactly the declared tokens reach the querier.
    let declared = CapabilitySet::new(
        header::Allow::of(["INVITE", "ACK", "CANCEL", "BYE", "OPTIONS"]),
        header::Supported::of(["timer"]),
    );
    let resp = build_options_health_response(&r, &ov(), &tagger, &req, &declared);
    assert_eq!(
        value(&resp, HeaderName::Allow).as_deref(),
        Some("INVITE, ACK, CANCEL, BYE, OPTIONS")
    );
    assert_eq!(value(&resp, HeaderName::Supported).as_deref(), Some("timer"));
}

/// A member that is present but **not ready** is pulled and never waited on
/// (ADR-0031 D1): its Reclaim flow may be unreachable, mid-bootstrap or lost,
/// and the endpoint may linger in the slice indefinitely (a pod stuck
/// `Terminating`), so the readiness gates exclude it while the puller keeps
/// running toward it.
#[tokio::test(start_paused = true)]
async fn a_not_ready_member_is_pulled_but_does_not_gate_readiness() {
    let clock = Clock::test_at(0);
    // No B server is ever spawned: B's Reclaim flow never connects and, before
    // the hard timer, is neither current nor bootstrap-complete.
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b_addr = addr(4);
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
    let membership =
        Arc::new(SimulatedMembership::with_clock(vec![Peer::new("B", "B")], clock.clone()));
    a_sup.start(membership.clone());
    tick(100).await;
    assert!(!a_sup.all_current(), "a ready peer still pending gates readiness (sanity)");
    assert!(!a_sup.all_bootstrapped());

    // B's endpoint flips not ready and stays in the slice.
    membership.set_conditions("B", false, true);
    tick(100).await;
    assert!(a_sup.is_running("B"), "a not-ready member is still pulled");
    assert_eq!(a_sup.peer_link("B"), PeerLink::Kept);
    assert!(a_sup.all_bootstrapped(), "a pulled-not-ready member does not gate bootstrap");
    assert!(a_sup.all_current(), "a pulled-not-ready member does not gate readiness");

    // Ready again before its hard timer: it gates once more.
    membership.set_conditions("B", true, false);
    tick(100).await;
    assert!(!a_sup.all_current(), "a ready peer that is still pending gates readiness again");
}
