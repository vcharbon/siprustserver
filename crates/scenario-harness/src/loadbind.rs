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

/// One Test-case check verdict projected into a sampled callflow page — the
/// dependency-light mirror of the e2e check engine's `CheckVerdict` (the engine
/// lives in `e2e-model`, which depends on this crate, so the binder renders
/// from this data shape instead). Rendered in the page's anomaly list, PASS and
/// FAIL alike (`passed` maps onto the anomaly's advisory flag).
#[derive(Debug, Clone)]
pub struct CheckNote {
    /// `check <on> <field>` — the anomaly's rule id column.
    pub name: String,
    /// The verdict's human-readable detail (what matched / why it failed).
    pub detail: String,
    pub passed: bool,
}

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
    /// Build a binder over a **real** UDP network — the load driver's production
    /// mode, driving an external SUT (the kind cluster VIP). `clock` is the
    /// process-wide shared clock (created ONCE at startup, not per call), so every
    /// call's recording sits on one monotonic-anchored axis; passing it in also
    /// keeps that axis in lockstep with the chaos-marker log. `record` selects
    /// whether this call's trace is captured. `recv_timeout` is real latency, so
    /// set it wide.
    pub fn real(clock: Clock, recv_timeout: Duration, record: bool) -> Self {
        Self::with_network(
            Arc::new(sip_net::RealSignalingNetwork::new()),
            clock,
            TransportKind::Live,
            recv_timeout,
            record,
        )
    }

    /// Build a binder over a caller-supplied network tagged `Live` — the load
    /// driver's mux mode (the network is a per-call `MuxNetwork`). `clock` is the
    /// shared process-wide clock (see [`real`](Self::real)); the driver passes the
    /// same instance for every call so all timelines and the chaos markers share
    /// one axis. `record` applies the existing recording layer on top.
    pub fn mux(
        network: Arc<dyn SignalingNetwork>,
        clock: Clock,
        recv_timeout: Duration,
        record: bool,
    ) -> Self {
        Self::with_network(network, clock, TransportKind::Live, recv_timeout, record)
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
            // Load lane stays on the RAW wire surface: `loadgen::mux::CallTxns`
            // already owns retransmit dedup ahead of the agent, and a second
            // (differently-keyed) dedup here would silently change load
            // semantics (newkahneed-034). The §17.1.1.3 ACK obligations (036
            // ask B) are independent of the view and apply here too — a load
            // body that rejects an INVITE never trips over the hop ACK.
            txn: Arc::new(crate::agent::TxnView::wire()),
            acks: Arc::new(crate::agent::AckObligations::default()),
        }
    }

    /// Render this call's recorded trace as a standalone callflow HTML page
    /// (`None` if the call was not sampled). Projects the recording through the
    /// SAME path the harness report uses (`to_sip_entries` → `sip_doc` →
    /// `seq_report::render_html`), so a sampled load call's flow renders exactly
    /// like a functional-test callflow.
    /// `banner` is an always-shown per-call context line (the resolved binding —
    /// the actual From/To used — on the load surface); it renders in the page
    /// header on PASS and FAIL alike. `detail` is the call's failure reason (the
    /// [`StepError`]/outcome string); when present on a NOT-passed call it is
    /// surfaced in the rendered page so the sampled callflow explains WHY it
    /// failed, not merely that it did.
    /// `chaos_markers` are injected-fault instants as `(wall_clock_epoch_ms,
    /// label)` — the load driver passes the recent `POST /chaos` markers so a
    /// sampled NOK flow renders each one (that falls in this call's window) as a
    /// Lifecycle band, making it obvious whether the kill landed during the call.
    /// Pass `&[]` when there is no chaos correlation.
    /// `checks` are the call's Test-case check verdicts (PASS and FAIL), each
    /// rendered as an entry in the page's anomaly list; pass `&[]` when the call
    /// carries no case checks.
    pub fn render_html(
        &self,
        title: &str,
        passed: bool,
        banner: Option<&str>,
        detail: Option<&str>,
        chaos_markers: &[(i64, String)],
        checks: &[CheckNote],
    ) -> Option<String> {
        let doc = self.seq_doc(title, passed, banner, detail, chaos_markers, checks)?;
        Some(seq_report::render_html(&doc))
    }

    /// Render this call's recorded trace as an SVG sequence diagram (`None` if not
    /// sampled).
    pub fn render_svg(&self, title: &str, passed: bool) -> Option<String> {
        let doc = self.seq_doc(title, passed, None, None, &[], &[])?;
        Some(seq_report::render_svg(&doc))
    }

    /// The non-advisory RFC 3261/3262/3264 findings over this call's recorded
    /// trace (empty if clean or the call was not sampled) — the load-driver
    /// analogue of the harness hard gate, surfaced as data instead of a panic so
    /// the driver can classify an otherwise-OK call as RFC-dirty. `allow` is the
    /// per-call waived rule-name set (the Test case's `allowViolations` — the
    /// load-surface analogue of [`Harness::allow_violation`]): findings from
    /// those rules are exempt, exactly like the harness hard gate skips waived
    /// rules. Pass an empty set for the historic behaviour.
    pub fn rfc_findings(&self, allow: &HashSet<String>) -> Vec<sip_net::RfcFinding> {
        match &self.recording {
            Some(rec) => sip_net::evaluate_rfc_findings(&rec.channel().snapshot())
                .into_iter()
                .filter(|f| !f.advisory && !allow.contains(&f.rule))
                .collect(),
            None => Vec::new(),
        }
    }

    /// The wire trace of this call's recording, projected exactly like
    /// `RunReport::entries` (empty when the call was not sampled) — the entries
    /// half of what the e2e check engine evaluates over (the anchors half rides
    /// the per-call `CallCtx`).
    pub fn recorded_entries(&self) -> Vec<sip_net::RecordedSipEntry> {
        match &self.recording {
            Some(rec) => sip_net::to_sip_entries(&rec.channel().snapshot()),
            None => Vec::new(),
        }
    }

    fn seq_doc(
        &self,
        title: &str,
        passed: bool,
        banner: Option<&str>,
        detail: Option<&str>,
        chaos_markers: &[(i64, String)],
        checks: &[CheckNote],
    ) -> Option<seq_report::SeqDoc> {
        let (recording, recorder) = (self.recording.as_ref()?, self.recorder.as_ref()?);
        let events = recording.channel().snapshot();
        let entries = sip_net::to_sip_entries(&events);
        let scenario = recorder.snapshot();
        // On a failure, surface the reason both as the doc description (the header
        // banner next to the FAIL status) and as an explicit anomaly in the list,
        // so the sampled NOK callflow says WHY, not just that it failed.
        let reason = detail.filter(|_| !passed);
        // The description = the always-shown banner (the resolved binding) plus,
        // on a failure, the reason — both in the header, PASS or FAIL.
        let description = match (banner, reason) {
            (Some(b), Some(r)) => Some(format!("{b} — {r}")),
            (Some(b), None) => Some(b.to_string()),
            (None, r) => r.map(str::to_string),
        };
        let mut extra: Vec<seq_report::Anomaly> = match reason {
            Some(d) => vec![seq_report::Anomaly {
                check: "call-result".to_string(),
                detail: d.to_string(),
                lane: None,
                endpoint: None,
                advisory: Some(false),
            }],
            None => Vec::new(),
        };
        // Test-case check verdicts, PASS and FAIL alike, so a sampled page shows
        // the case's oracle next to the flow (a passed check is advisory-styled).
        for c in checks {
            extra.push(seq_report::Anomaly {
                check: c.name.clone(),
                detail: if c.passed {
                    format!("PASS: {}", c.detail)
                } else {
                    format!("FAIL: {}", c.detail)
                },
                lane: None,
                endpoint: None,
                advisory: Some(c.passed),
            });
        }
        // The load binder records on `Clock::system()`, so `sent_ms` is real
        // wall-clock epoch ms → `wall_clock: true` renders absolute UTC and the
        // chaos markers (also epoch ms) drop onto the timeline at the kill instant.
        let overlay = crate::report::project::TimelineOverlay {
            wall_clock: true,
            markers: chaos_markers.to_vec(),
        };
        Some(crate::report::project::sip_doc_with_overlay(
            title,
            description.as_deref(),
            &entries,
            &scenario,
            passed,
            &extra,
            &overlay,
        ))
    }
}
