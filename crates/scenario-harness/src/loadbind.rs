//! `AgentBinder` — a **`Send`** agent factory for the load-test driver.
//!
//! # Why this exists
//!
//! [`Harness`](crate::Harness) is the fluent session most tests use, but it is
//! **`!Send`**: it holds `Rc<RefCell<…>>` (the `allow_violation` set and the
//! anchor list) plus the Drop-time RFC panic gate. That is fine for a
//! `#[tokio::test]` that drives one call on one task, but a *load* generator must
//! run thousands of calls concurrently on a multi-threaded runtime — which only
//! accepts `Send` futures.
//!
//! The key asymmetry: only the `Harness` *wrapper* is `!Send`. The [`Agent`] it
//! hands out holds only `Send` fields (`Arc` endpoint + id source), and the
//! recording machinery underneath ([`Recorder`], the recording decorator) is all
//! `Arc<Mutex<…>>` — also `Send`. So `AgentBinder` reproduces the load-bearing
//! half of [`Harness::build`] (recorder + the RFC-contract-wrapped network +
//! `Agent` construction) **without** the `Rc` fields and the panic gate, yielding
//! a `Send + Sync` factory.
//!
//! Each load call constructs its own short-lived binder (so the recording buffer
//! is freed when the binder drops at call end — flat memory) and decides at
//! construction whether to **record**: a sampled call wraps its network in the
//! recording + RFC-audit decorators so its callflow can be projected to HTML;
//! the unsampled majority binds the raw network directly, for zero per-call
//! recording overhead. See `loadgen::report` for the sampling gate.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{NetworkTag, Recorder, RunContext, TransportKind};
use sip_clock::Clock;
use sip_net::{
    with_all_contracts, BindUdpOpts, ScopedAuditOptions, SignalingNetwork, SimulatedSignalingNetwork,
};

use crate::agent::{decide_rr_fold, Agent, Ids};

/// A `Send + Sync` factory that binds [`Agent`]s on a recording-wrapped (or, when
/// not recording, raw) network — the load-driver analogue of [`Harness`]
/// (crate::Harness) minus the `!Send` `Rc` state and the Drop-time panic gate.
pub struct AgentBinder {
    /// The network agents bind on: the RFC-contract + recording decorator stack
    /// when recording, else the raw network (zero overhead).
    network: Arc<dyn SignalingNetwork>,
    /// Present only when recording — the handle to snapshot the trace.
    recording: Option<sip_net::RecordingSignalingNetwork>,
    /// Present only when recording — the lane registry + event channel owner.
    recorder: Option<Recorder>,
    ids: Arc<Ids>,
    recv_timeout: Duration,
}

impl AgentBinder {
    /// Build a binder over a **real** UDP network and the system wall clock — the
    /// load driver's production mode, driving an external SUT (the kind cluster
    /// VIP). `record` selects whether this call's trace is captured for the
    /// callflow report. `recv_timeout` is real latency, so set it wide.
    pub fn real(recv_timeout: Duration, record: bool) -> Self {
        Self::with_network(
            Arc::new(sip_net::RealSignalingNetwork::new()),
            Clock::system(),
            TransportKind::Live,
            recv_timeout,
            record,
        )
    }

    /// Build a binder over a caller-supplied **real-clock** network tagged
    /// `Live` — the load driver's mux mode (the network is a per-call
    /// `MuxNetwork`). `record` applies the existing recording layer on top.
    pub fn mux(network: Arc<dyn SignalingNetwork>, recv_timeout: Duration, record: bool) -> Self {
        Self::with_network(network, Clock::system(), TransportKind::Live, recv_timeout, record)
    }

    /// Build a binder over a fresh in-memory simulated network (deterministic
    /// timestamps via [`Clock::test_at`]) — for the load driver's own fake-network
    /// smoke test. NOTE: to drive an in-process SUT the binder and the SUT must
    /// share ONE network instance; use [`with_network`](Self::with_network) with
    /// the SUT's network for that. This convenience binds an isolated fabric
    /// (agents can only talk to each other).
    pub fn fake(transit_delay_ms: u64, recv_timeout: Duration, record: bool) -> Self {
        Self::with_network(
            Arc::new(SimulatedSignalingNetwork::new(transit_delay_ms.max(1))),
            Clock::test_at(0),
            TransportKind::Fake,
            recv_timeout,
            record,
        )
    }

    /// Build a binder over a **caller-supplied** simulated network + clock,
    /// tagged `Fake` — the seam the fake smoke test uses to bind agents on the
    /// SAME `SimulatedSignalingNetwork` an in-process `B2buaSut` runs on (so the
    /// test does not need to name `TransportKind` itself).
    pub fn shared_fake(
        network: Arc<dyn SignalingNetwork>,
        clock: Clock,
        recv_timeout: Duration,
        record: bool,
    ) -> Self {
        Self::with_network(network, clock, TransportKind::Fake, recv_timeout, record)
    }

    /// Core constructor over a **caller-supplied** network + clock — the seam that
    /// lets the fake smoke test bind agents on the SAME `SimulatedSignalingNetwork`
    /// an in-process `B2buaSut` runs on (a separate instance is an isolated fabric
    /// they can't talk across). Mirrors [`Harness::with_network_and_clock`].
    pub fn with_network(
        network: Arc<dyn SignalingNetwork>,
        clock: Clock,
        transport_kind: TransportKind,
        recv_timeout: Duration,
        record: bool,
    ) -> Self {
        let ids = Arc::new(Ids(AtomicU64::new(1)));
        if !record {
            return Self { network, recording: None, recorder: None, ids, recv_timeout };
        }
        // Recording path: wrap the raw network in the recorder + the default RFC
        // 3261/3262/3264 wire-invariant suite, exactly as `Harness::build` does,
        // so a sampled call's trace can be projected and RFC-audited.
        let recorder = Recorder::with_clock(transport_kind, clock);
        let audit_opts = ScopedAuditOptions {
            rules: sip_net::rfc_peer_rules(),
            cross_message_rules: sip_net::rfc_cross_message_rules(),
            ..Default::default()
        };
        let wrapped = with_all_contracts(
            network,
            recorder.clone(),
            RunContext::TestWithRecorder,
            audit_opts,
            true,
        );
        Self {
            network: wrapped.network,
            recording: Some(wrapped.recording),
            recorder: Some(recorder),
            ids,
            recv_timeout,
        }
    }

    /// Re-seed the shared branch/tag/Call-ID counter — MANDATORY against a real,
    /// stateful, shared SUT (the cluster), where reusing identifiers across runs
    /// makes the SUT's transaction layer absorb the new INVITE as a retransmit
    /// (RFC 3261 §17.2.3 / §8.1.1.4). See [`Harness::seed_ids`].
    pub fn seed_ids(&self, seed: u64) {
        self.ids.0.store(seed, Ordering::Relaxed);
    }

    /// Whether this binder records (the call was sampled).
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Bind a named UA at `addr` — the load-driver analogue of
    /// [`Harness::agent`](crate::Harness). Role-tagged `{Uac, Uas}` (a test UA
    /// originates and answers, never proxies). Bind with port `0` to let the OS
    /// assign a free port against a real network, then read [`Agent::addr`].
    pub async fn agent(&self, name: impl Into<String>, addr: &str) -> Agent {
        let name = name.into();
        let addr: SocketAddr =
            addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        if let Some(recorder) = &self.recorder {
            recorder.register_lane(addr, name.clone(), NetworkTag::Ext);
        }
        let roles = HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]);
        let ep = self
            .network
            .bind_udp(BindUdpOpts::new(addr, 64).with_roles(roles))
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        Agent {
            uri: format!("sip:{name}@{}", addr.ip()),
            rr_fold: decide_rr_fold(&name),
            name,
            addr,
            ep: Arc::from(ep),
            ids: self.ids.clone(),
            recv_timeout: self.recv_timeout,
        }
    }

    /// Render this call's recorded trace as a standalone callflow HTML page
    /// (`None` if the call was not sampled). Projects the recording through the
    /// SAME path the harness report uses (`to_sip_entries` → `sip_doc` →
    /// `seq_report::render_html`), so a sampled load call's flow renders exactly
    /// like a functional-test callflow.
    pub fn render_html(&self, title: &str, passed: bool) -> Option<String> {
        let doc = self.seq_doc(title, passed)?;
        Some(seq_report::render_html(&doc))
    }

    /// Render this call's recorded trace as an SVG sequence diagram (`None` if not
    /// sampled).
    pub fn render_svg(&self, title: &str, passed: bool) -> Option<String> {
        let doc = self.seq_doc(title, passed)?;
        Some(seq_report::render_svg(&doc))
    }

    /// The non-advisory RFC 3261/3262/3264 findings over this call's recorded
    /// trace (empty if clean or the call was not sampled) — the load-driver
    /// analogue of the harness hard gate, surfaced as data instead of a panic so
    /// the driver can classify an otherwise-OK call as RFC-dirty.
    pub fn rfc_findings(&self) -> Vec<sip_net::RfcFinding> {
        match &self.recording {
            Some(rec) => sip_net::evaluate_rfc_findings(&rec.channel().snapshot())
                .into_iter()
                .filter(|f| !f.advisory)
                .collect(),
            None => Vec::new(),
        }
    }

    fn seq_doc(&self, title: &str, passed: bool) -> Option<seq_report::SeqDoc> {
        let (recording, recorder) = (self.recording.as_ref()?, self.recorder.as_ref()?);
        let events = recording.channel().snapshot();
        let entries = sip_net::to_sip_entries(&events);
        let scenario = recorder.snapshot();
        Some(crate::report::project::sip_doc(
            title, None, &entries, &scenario, passed, &[],
        ))
    }
}
