//! The load driver: a CPS governor that spawns one `Send` task per call onto a
//! shared multi-threaded runtime, bounded by a max-in-flight semaphore, picking
//! scenarios by weighted random. Each per-call task mints a correlation token,
//! binds its agents on the **mux** (one socket per defined endpoint, many
//! dialogs demuxed), runs the scenario inside a `catch_unwind` boundary (a panic
//! is a *counted* failure, never a worker abort), tears the call down
//! (CANCEL/BYE) however it ended, classifies the result, and records it —
//! optionally projecting a sampled callflow (recording layered on the mux).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use e2e_model::{ScenarioInputs, ShapeDescriptor, ShapeRegistry};
use futures::FutureExt;
use scenario_harness::{AgentBinder, EgressPolicy};
use sip_clock::Clock;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::case::LoadCase;
use crate::chaos::{ChaosLog, ChaosTag};
use crate::class::{CallOutcome, ResultClass};
use crate::ctx::{CallCtx, CallEnv};
use crate::mux::{prefix_leg_picker, CallRouting, Correlation, MuxCore};
use crate::rate::{Governor, RateHandle};
use crate::report::{RenderedSample, Reporter};
use crate::scenarios::LoadScenario;
use crate::scope::CallScope;

/// The mux transport the driver binds calls on.
pub struct MuxTransport {
    pub core: Arc<MuxCore>,
    /// The caller (UAC) endpoint address.
    pub uac_addr: SocketAddr,
    /// The callee (UAS) endpoint address (the SUT routes the b-leg here).
    pub uas_addr: SocketAddr,
    /// The transfer-target (REFER) endpoint address.
    pub refer_addr: SocketAddr,
    /// How the correlation token travels through the SUT.
    pub correlation: Correlation,
    /// Per-recv wall-clock timeout.
    pub recv_timeout: Duration,
    /// The one process-wide clock every per-call binder records on (created once
    /// at startup, shared with the chaos log), so all call timelines — and the
    /// chaos markers — sit on a single monotonic-anchored axis.
    pub clock: Clock,
}

/// Static per-call routing config (shared `Arc` across all calls).
pub struct CallConfig {
    /// The address the initial INVITE routes through (SUT / VIP).
    pub via: SocketAddr,
    /// How this run's layout realizes a logical INVITE on the wire — the
    /// environment axis shared with the e2e framework (`EndpointConfig.egress`):
    /// [`EgressPolicy::Transparent`] when the SUT routes the callee itself,
    /// [`EgressPolicy::ApiCallPin`] to pin the b-leg back to the static `uas`
    /// endpoint (`--route-pin-to-uas`). Replaces the hand-rolled
    /// route/refer `X-Api-Call` pins.
    pub egress: EgressPolicy,
    pub options_hold: Duration,
    pub options_cadence: Duration,
    /// Realistic ring time (callee 180→200 dwell). `0` = answer immediately.
    pub ring_delay: Duration,
    /// Post-connect talk time before BYE (basic call). `0` = hang up immediately.
    pub talk_time: Duration,
    /// Spacing held before and after a re-INVITE. `0` = back-to-back.
    pub reinvite_gap: Duration,
    /// Total hold of a long recorded call, split either side of its OPTIONS ping.
    pub long_hold: Duration,
    /// After a *failed* call's a-leg is torn down, how long to drain-and-200 the
    /// in-process callee legs so the SUT closes its relayed b-leg promptly.
    pub teardown_quiesce: Duration,
}

/// Robustness knobs applied per call, resolved per scenario (a global default
/// overridden by any per-scenario entry). Both default off, so an un-tuned run is
/// byte-for-byte the historic behaviour.
#[derive(Clone, Copy, Debug, Default)]
pub struct CallTuning {
    /// Simulated packet-drop probability on this call's mux legs (0 = off). Each
    /// datagram is independently dropped; the SUT (and, when enabled,
    /// auto-retransmit) is what recovers the call.
    pub drop_rate: f64,
    /// Whether the harness auto-retransmits per real SIP timers (Timer A/E for
    /// requests, 2xx-until-ACK for answers) on this call — so a rare drop is
    /// recovered instead of failing the call.
    pub retransmit: bool,
}

/// Driver construction config.
pub struct DriverCfg {
    pub cps: f64,
    pub duration: Duration,
    pub max_in_flight: usize,
    pub seed: u64,
    pub call: CallConfig,
    /// The robustness knobs applied to every call unless a per-scenario entry
    /// overrides them.
    pub default_tuning: CallTuning,
    /// Per-scenario-id overrides of [`Self::default_tuning`] (keyed by the
    /// scenario's stable id / CLI name).
    pub tuning: HashMap<String, CallTuning>,
}

/// One scenario mix entry: the shape's **report/metrics id** + load attributes
/// (from its [`ShapeDescriptor`] — the shape's ONE declaration), the load body,
/// its pick weight, and an optional attached Test case
/// (`--scenario name=w,case=<path.json>` / the global `--case`) whose binding
/// pool drives per-call identities and dwells.
///
/// Build from the unified registry ([`MixEntry::by_id`] /
/// [`MixEntry::default_mix`] / [`MixEntry::failure_mix`]); plain
/// `(scenario, weight)` tuples still convert for hand-rolled test bodies
/// (attributes default off, id from the body).
#[derive(Clone)]
pub struct MixEntry {
    /// The report/metrics id — the DESCRIPTOR's id (may differ from the body's
    /// intrinsic `id()` when one body serves several shapes, e.g. `basic_call_em`).
    pub id: crate::scenarios::ScenarioId,
    pub scenario: Arc<dyn LoadScenario>,
    pub weight: f64,
    pub case: Option<Arc<LoadCase>>,
    /// Bind a third (transfer-target) callee leg for this shape's calls.
    pub needs_charlie: bool,
    /// Bind a SECOND callee receiver (`bob2`) sharing the callee socket,
    /// demuxed by the R-URI-user leg picker (the rerouting shapes).
    pub needs_bob2: bool,
    /// Stamp `Resource-Priority: esnet.0` (the SUT force-admits under overload).
    pub emergency: bool,
    /// **Deferred-by-design auth adapter** (see
    /// `scenario_harness::realcall::auth`). `None` (the default) = no RFC 3261
    /// §22.2 retry; a `401`/`407` classifies as `status_401`/`status_407`. Set it
    /// (via [`MixEntry::with_challenge_responder`]) to point the fleet at a
    /// challenging registrar/proxy once digest lands — no CLI flag mints one yet.
    pub challenge_responder: Option<Arc<dyn scenario_harness::realcall::ChallengeResponder>>,
}

impl From<(Arc<dyn LoadScenario>, f64)> for MixEntry {
    fn from((scenario, weight): (Arc<dyn LoadScenario>, f64)) -> Self {
        MixEntry {
            id: scenario.id(),
            scenario,
            weight,
            case: None,
            needs_charlie: false,
            needs_bob2: false,
            emergency: false,
            challenge_responder: None,
        }
    }
}

impl MixEntry {
    /// Build a mix entry from a shape's descriptor (minting its load body from
    /// the per-run `inputs`). `None` when the shape has no load body
    /// (functional-only).
    pub fn from_shape(
        shape: &ShapeDescriptor,
        inputs: &ScenarioInputs,
        weight: f64,
    ) -> Option<Self> {
        let scenario = shape.load_scenario(inputs)?;
        Some(MixEntry {
            id: shape.id,
            scenario,
            weight,
            case: None,
            needs_charlie: shape.needs_charlie,
            needs_bob2: shape.needs_bob2,
            emergency: shape.emergency,
            challenge_responder: None,
        })
    }

    /// Resolve a shape by id in the registry (the CLI's `--scenario name=weight`).
    /// `None` for an unknown id or a functional-only shape.
    pub fn by_id(
        registry: &ShapeRegistry,
        id: &str,
        inputs: &ScenarioInputs,
        weight: f64,
    ) -> Option<Self> {
        registry.get(id).and_then(|d| Self::from_shape(d, inputs, weight))
    }

    /// The shipped DEFAULT mix (every shape with a `default_weight`,
    /// basic-heavy like real traffic).
    pub fn default_mix(registry: &ShapeRegistry, inputs: &ScenarioInputs) -> Vec<Self> {
        registry
            .default_mix()
            .into_iter()
            .filter_map(|d| Self::from_shape(d, inputs, d.default_weight.unwrap_or(1.0)))
            .collect()
    }

    /// The voluntarily-failing mix (one shape per post-call-cleanup teardown
    /// path), for the no-leak cleanup-coverage test.
    pub fn failure_mix(registry: &ShapeRegistry, inputs: &ScenarioInputs) -> Vec<Self> {
        registry
            .failure_mix()
            .into_iter()
            .filter_map(|d| Self::from_shape(d, inputs, d.failure_weight.unwrap_or(1.0)))
            .collect()
    }

    /// Attach a Test case (binding pool → per-call identities + dwells).
    pub fn with_case(mut self, case: Option<Arc<LoadCase>>) -> Self {
        self.case = case;
        self
    }

    /// Attach the deferred-by-design auth adapter (see the field docs). The load
    /// driver folds it into every call's `CallEnv`; the default is `None`.
    pub fn with_challenge_responder(
        mut self,
        responder: Option<Arc<dyn scenario_harness::realcall::ChallengeResponder>>,
    ) -> Self {
        self.challenge_responder = responder;
        self
    }
}

/// The load driver.
pub struct Driver {
    /// The live-tunable offered rate (milli-cps under the hood). Seeded from
    /// `cfg.cps`; `POST /rate` re-targets it and the governor re-anchors its grid.
    rate: RateHandle,
    duration: Duration,
    max_in_flight: usize,
    seed: u64,
    reporter: Arc<Reporter>,
    scenarios: Vec<MixEntry>,
    total_weight: f64,
    sem: Arc<Semaphore>,
    transport: Arc<MuxTransport>,
    call: Arc<CallConfig>,
    default_tuning: CallTuning,
    tuning: HashMap<String, CallTuning>,
    /// Optional chaos-marker log: when set, each finished call is classified
    /// near/clear against the injected-fault markers (the chaos correlation).
    chaos: Option<Arc<ChaosLog>>,
}

/// Process-wide per-call id-seed source: a unique 100k-wide window per call so a
/// fresh binder (whose `Ids` restart at 1) never mints Call-IDs colliding with
/// another in-flight or prior call against the same stateful SUT.
fn next_seed(base: u64) -> u64 {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    base.wrapping_add(SERIAL.fetch_add(1, Ordering::Relaxed).wrapping_mul(100_000))
        .max(1)
}

/// A random per-call correlation token, formatted as a valid SIP user-part.
fn mint_token() -> String {
    format!("lg{}", uuid::Uuid::new_v4().simple())
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn panic_msg(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

impl Driver {
    pub fn new<M: Into<MixEntry>>(
        cfg: DriverCfg,
        scenarios: Vec<M>,
        reporter: Arc<Reporter>,
        transport: Arc<MuxTransport>,
    ) -> Self {
        let scenarios: Vec<MixEntry> = scenarios.into_iter().map(Into::into).collect();
        assert!(!scenarios.is_empty(), "loadgen needs at least one scenario");
        let total_weight = scenarios.iter().map(|e| e.weight).sum();
        Self {
            rate: RateHandle::new(cfg.cps),
            duration: cfg.duration,
            max_in_flight: cfg.max_in_flight,
            seed: cfg.seed.max(1),
            reporter,
            scenarios,
            total_weight,
            sem: Arc::new(Semaphore::new(cfg.max_in_flight)),
            transport,
            call: Arc::new(cfg.call),
            default_tuning: cfg.default_tuning,
            tuning: cfg.tuning,
            chaos: None,
        }
    }

    /// Resolve the [`CallTuning`] for a scenario: its per-id override if any, else
    /// the global default.
    fn tuning_for(&self, id: &str) -> CallTuning {
        self.tuning.get(id).copied().unwrap_or(self.default_tuning)
    }

    /// Attach a [`ChaosLog`] so finished calls are tagged near/clear against the
    /// injected-fault markers (fed by the `POST /chaos` endpoint).
    pub fn with_chaos(mut self, chaos: Arc<ChaosLog>) -> Self {
        self.chaos = Some(chaos);
        self
    }

    pub fn reporter(&self) -> &Arc<Reporter> {
        &self.reporter
    }

    /// A clone of the live rate handle — hand it to the `/rate` HTTP surface (and
    /// read the current target for the `loadgen_target_cps` gauge). Re-targeting it
    /// re-anchors the governor's grid on the next slot.
    pub fn rate_handle(&self) -> RateHandle {
        self.rate.clone()
    }

    fn pick(&self, rng: &mut u64) -> &MixEntry {
        let r = (xorshift(rng) as f64 / u64::MAX as f64) * self.total_weight;
        let mut acc = 0.0;
        for entry in &self.scenarios {
            acc += entry.weight;
            if r <= acc {
                return entry;
            }
        }
        self.scenarios.last().unwrap()
    }

    /// Run the load for the configured duration, then drain in-flight calls.
    ///
    /// The spawn cadence is the [`Governor`] — a re-anchoring fixed-grid CPS
    /// scheduler that reads the shared [`RateHandle`] on every slot, so
    /// `POST /rate` re-targets it live (a rate change re-anchors the grid so a cut
    /// fires no catch-up burst and a raise takes effect within one slot; `cps == 0`
    /// pauses new-call admission while in-flight calls run untouched). A catch-up
    /// burst is bounded by the max-in-flight semaphore — the excess is shed+counted,
    /// never an unbounded spawn storm.
    pub async fn run(&self) {
        let mut rng = self.seed;
        let mut governor = Governor::new(self.rate.clone(), self.duration);

        while governor.next_slot().await.is_some() {
            let entry = self.pick(&mut rng).clone();
            let permit = match self.sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    self.reporter.inc_shed(entry.id);
                    continue;
                }
            };
            let tuning = self.tuning_for(entry.id);
            tokio::spawn(run_one(
                entry,
                self.reporter.clone(),
                self.transport.clone(),
                self.call.clone(),
                self.seed,
                tuning,
                self.chaos.clone(),
                permit,
            ));
        }

        // Drain: acquiring every permit blocks until all in-flight calls release.
        let _ = self.sem.acquire_many(self.max_in_flight as u32).await;
    }
}

/// One call, start to finish. `Send + 'static`, so it runs on the shared
/// multi-threaded runtime.
#[allow(clippy::too_many_arguments)] // per-call context threaded explicitly (no shared struct)
async fn run_one(
    entry: MixEntry,
    reporter: Arc<Reporter>,
    transport: Arc<MuxTransport>,
    call: Arc<CallConfig>,
    seed_base: u64,
    tuning: CallTuning,
    chaos: Option<Arc<ChaosLog>>,
    _permit: OwnedSemaphorePermit,
) {
    reporter.inc_inflight();
    let MixEntry {
        id,
        scenario,
        case,
        needs_charlie,
        needs_bob2,
        emergency,
        challenge_responder,
        weight: _,
    } = entry;

    // Resolve THIS call's binding from the attached Test case (pool walk +
    // token expansion): the core From/To/R-URI to fold into the outgoing
    // INVITE, the per-call dwell overrides over the global CallConfig
    // defaults, the sampled-page banner, and the resolved input the case's
    // checks bind `${input.*}` against. No case → all defaults.
    let resolved = case.as_ref().map(|c| c.resolve());
    let (core, dwells, banner, resolved_input) = match resolved {
        Some(r) => (r.core, r.dwells, Some(r.banner), Some(r.input)),
        None => (Default::default(), Default::default(), None, None),
    };

    // Two-tier callee demux. One correlation token per CALL: alice stamps it on
    // her INVITE (per the run's strategy — relayed header or To-user) and the SUT
    // carries it onto every downstream leg, so every callee leg shares it. That
    // token is the FIRST tier — it selects the call INSTANCE (mux `by_token`).
    //
    // Every callee-side leg (bob, the rerouting `bob2`, the transfer `charlie`)
    // shares ONE socket (`transport.uas_addr`); the SECOND tier — WHICH leg of
    // this instance — is the R-URI-prefix leg picker, since the egress addresses
    // each callee by role (`sip:bob2@…`, `sip:charlie@…`: the reroute plan's
    // `new_ruri`, the transfer's Refer-To user). So bob1/bob2/charlie need no
    // per-leg socket — they are "distinguished by prefix".
    let token = mint_token();
    let mut callee_labels: Vec<&str> = vec!["bob"];
    if needs_bob2 {
        callee_labels.push("bob2");
    }
    if needs_charlie {
        callee_labels.push("charlie");
    }
    let mut routing = CallRouting::new(token.clone());
    for label in &callee_labels {
        routing = routing.leg(transport.uas_addr, *label);
    }
    if callee_labels.len() > 1 {
        routing = routing.picker(transport.uas_addr, prefix_leg_picker(callee_labels.iter().copied()));
    }

    let record = reporter.should_record(id);
    // Simulated packet loss + auto-retransmit both ride the mux transport (the mux
    // dispatcher is the background pump that reacts to inbound datagrams). The loss
    // RNG is seeded off the call seed so a run is reproducible; 0 rate is a no-op,
    // and retransmit off leaves the transport untouched.
    let mux_net = transport.core.network_tuned(
        routing,
        tuning.drop_rate,
        tuning.retransmit,
        next_seed(seed_base),
    );
    let binder =
        AgentBinder::mux(Arc::new(mux_net), transport.clock.clone(), transport.recv_timeout, record);
    binder.seed_ids(next_seed(seed_base));

    let alice = binder.agent("alice", &transport.uac_addr.to_string()).await;
    // Bind order is load-bearing on the shared callee socket: the mux assigns
    // receivers in leg-declaration order, so bind bob → bob2 → charlie to match
    // the `callee_labels` order above. All callee legs share `uas_addr`; the
    // prefix picker demuxes them (`transport.refer_addr` is retained as a bound
    // endpoint for the CLI's alice/bob/charlie role set, but no longer carries a
    // separate transfer socket).
    let bob = binder.agent("bob", &transport.uas_addr.to_string()).await;
    let bob2 = if needs_bob2 {
        Some(binder.agent("bob2", &transport.uas_addr.to_string()).await)
    } else {
        None
    };
    let charlie = if needs_charlie {
        Some(binder.agent("charlie", &transport.uas_addr.to_string()).await)
    } else {
        None
    };

    // A SAMPLED call collects message anchors (ADR-0019) so the attached Test
    // case's checks can resolve `<agent>.<anchor>` over its recorded trace;
    // on the unsampled majority tagging stays a single atomic load.
    let ctx = CallCtx::new();
    if record {
        ctx.enable_anchor_collection();
    }

    let env = CallEnv {
        alice: &alice,
        bob: &bob,
        bob2: bob2.as_ref(),
        charlie: charlie.as_ref(),
        via: call.via,
        stamp: transport.correlation.stamp(&token),
        token,
        emergency,
        core,
        egress: call.egress.clone(),
        // Deferred-by-design auth adapter (default None): the mix entry's
        // responder, folded into every call so the §22.2 retry is a one-object
        // opt-in, not a choreography change.
        challenge_responder: challenge_responder.clone(),
        options_hold: call.options_hold,
        // Per-call dwells: the resolved case extras override the global
        // CallConfig defaults knob-by-knob (unset knobs keep the default).
        options_cadence: dwells.options_cadence.unwrap_or(call.options_cadence),
        ring_delay: dwells.ring_delay.unwrap_or(call.ring_delay),
        talk_time: dwells.talk_time.unwrap_or(call.talk_time),
        reinvite_gap: dwells.reinvite_gap.unwrap_or(call.reinvite_gap),
        long_hold: dwells.long_hold.unwrap_or(call.long_hold),
    };
    let scope = CallScope::new();

    let result = AssertUnwindSafe(scenario.run(&env, &scope, &ctx)).catch_unwind().await;

    // Cleanup FIRST (release any dialog on the SUT), then classify/report.
    scope.teardown().await;
    let failed = !matches!(result, Ok(Ok(())));
    if failed && !call.teardown_quiesce.is_zero() {
        bob.quiesce(call.teardown_quiesce).await;
        if let Some(b2) = &bob2 {
            b2.quiesce(call.teardown_quiesce).await;
        }
        if let Some(c) = &charlie {
            c.quiesce(call.teardown_quiesce).await;
        }
    }

    // The per-call audit policy: the attached case's `allowViolations` exempt
    // those rule names from the RFC hard gate (the load analogue of
    // `Harness::allow_violation`); no case / empty list = the full audit.
    static NO_WAIVERS: std::sync::LazyLock<std::collections::HashSet<String>> =
        std::sync::LazyLock::new(std::collections::HashSet::new);
    let allow = case.as_ref().map(|c| c.allow_violations()).unwrap_or(&NO_WAIVERS);

    let outcome = match result {
        Ok(Ok(())) => {
            let findings = binder.rfc_findings(allow);
            if findings.is_empty() {
                CallOutcome::Ok
            } else {
                CallOutcome::RfcAuditFail(
                    findings.iter().map(|f| f.detail.clone()).collect::<Vec<_>>().join("; "),
                )
            }
        }
        Ok(Err(e)) => CallOutcome::Step(e),
        Err(payload) => CallOutcome::Panic(panic_msg(payload)),
    };

    // Test-case CHECKS — evaluated on SAMPLED, otherwise-OK calls only (the
    // per-sample oracle; a non-sampled call has no recording to check, and a
    // failed/RFC-dirty call already explains itself). The verdicts (pass AND
    // fail) render on the sampled callflow page; any failed check reclassifies
    // the call to `check_fail`.
    let verdicts: Vec<e2e_model::CheckVerdict> = match (&outcome, case.as_ref(), &resolved_input) {
        (CallOutcome::Ok, Some(c), Some(input)) if record && c.has_checks() => {
            c.evaluate(&binder.recorded_entries(), &ctx.take_anchors(), input, call.via)
        }
        _ => Vec::new(),
    };
    // Fold this sampled call's aggregate check verdict into the per-scenario
    // check-verdict tally (the `checks` summary of the machine-readable index):
    // only when checks were actually evaluated, so a case-less or unsampled call
    // never skews it.
    if !verdicts.is_empty() {
        reporter.record_checks(id, verdicts.iter().all(|v| v.passed));
    }
    let outcome = if verdicts.iter().any(|v| !v.passed) {
        let failed = verdicts
            .iter()
            .filter(|v| !v.passed)
            .map(|v| format!("{} {}: {}", v.on, v.field, v.detail))
            .collect::<Vec<_>>()
            .join("; ");
        CallOutcome::CheckFail(failed)
    } else {
        outcome
    };
    let check_notes: Vec<scenario_harness::CheckNote> = verdicts
        .iter()
        .map(|v| scenario_harness::CheckNote {
            name: format!("check {} {}", v.on, v.field),
            detail: v.detail.clone(),
            passed: v.passed,
        })
        .collect();

    let class = ResultClass::from(&outcome);
    let e2e = ctx.elapsed();
    let checkpoints = ctx.take_checkpoints();
    // Fold this call's 18x outcome into the cross-call ringing-delivery gate (a
    // dropped non-PRACK 18x is expected, so it is a rate, not a per-call failure).
    reporter.record_ringing(ctx.ringing());

    // Classify the call near/clear against the injected-fault markers using the
    // per-phase rule: `Near` iff a fault landed on a dialog-state transition
    // (±phase_tolerance — no time to propagate, SIP retransmission recovers it) or
    // mid-setup. A call stably connected across the fault stays `Clear`. No log →
    // always Clear (the smoke tests + a chaos-less run).
    //
    // BUT a protocol-defect class (RfcAuditFail/WrongMethod/Unexpected/…) is NEVER
    // excused even if a transition coincided with the kill: the post-reboot reclaim
    // bug CONNECTS near the kill yet desyncs at teardown, so excusing it on the
    // near-kill connect would HIDE it (proven 2026-06-29 — neither self-heal path
    // recovers). Only transient/transport failures (the real kill collateral) are
    // eligible. See `ResultClass::chaos_excusable`.
    let chaos_tag = match chaos.as_ref() {
        Some(c) if class.chaos_excusable() => {
            c.classify_call(ctx.start_instant(), Instant::now(), &ctx.phases())
        }
        _ => ChaosTag::Clear,
    };

    let sample = if reporter.wants_sample(id, &class, chaos_tag) {
        let detail = phase_annotated_detail(outcome.detail(), &ctx);
        // Thread the failure reason into the rendered callflow so a sampled NOK
        // page explains WHY (header banner + an explicit anomaly), not just "FAIL".
        let html = if binder.is_recording() {
            // Pass the chaos markers so a sampled NOK flow renders the kill
            // instant(s) in its window as Lifecycle bands (absolute UTC, on the
            // call's wall-clock timeline) — see `ChaosLog::markers`.
            let markers = chaos.as_ref().map(|c| c.markers()).unwrap_or_default();
            // The banner (the resolved binding — the actual From/To used) shows
            // in the page header on PASS and FAIL alike; the failure detail
            // stays FAIL-only; the case's check verdicts render pass AND fail.
            // Metrics labels stay scenario-keyed.
            binder.render_html(
                id,
                class.is_ok(),
                banner.as_deref(),
                detail.as_deref(),
                &markers,
                &check_notes,
            )
        } else {
            None
        };
        if html.is_some() || !class.is_ok() {
            Some(RenderedSample {
                html,
                detail,
                e2e_ms: e2e.as_secs_f64() * 1000.0,
            })
        } else {
            None
        }
    } else {
        None
    };

    reporter.record(id, &outcome, e2e, &checkpoints, sample, chaos_tag);
    reporter.dec_inflight();
}

/// Append the call's lifecycle phase trail (`connected@1234ms`, `reinvited@…`) to
/// a NOK sample's one-line detail, so a sampled failing flow says WHICH phase it
/// reached before it failed (and, with the chaos correlation, near which fault).
/// `None`/empty detail (an OK call) is left untouched.
fn phase_annotated_detail(detail: Option<String>, ctx: &CallCtx) -> Option<String> {
    let detail = detail?;
    let phases = ctx.phases();
    if phases.is_empty() {
        return Some(detail);
    }
    let start = ctx.start_instant();
    let trail = phases
        .iter()
        .map(|(name, at)| format!("{name}@{}ms", at.saturating_duration_since(start).as_millis()))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{detail} [phases: {trail}]"))
}

/// A minimal HTTP/1.1 server (hand-rolled over `TcpListener` — no hyper
/// dependency). Routes:
///
/// - **`GET /metrics`** (and any other GET) → `render()`, the Prometheus surface.
/// - **`POST /chaos?type=<kind>&target=<who>[&ts=<ms>]`** (when a [`ChaosLog`] is
///   attached) → records a fault marker so finished calls get auto-classified
///   near/clear. With `ts` (Unix epoch ms of the actual kill, supplied by the
///   chaos driver) the marker is back-dated to the kill instant — robust to
///   port-forward latency on the flag path; without it it lands at receipt instant.
/// - **`POST /rate?cps=<float>`** (when a [`RateHandle`] is attached) → re-targets
///   the offered call rate live (clamped to `>= 0`; `0` pauses new-call admission).
///   The governor re-anchors its grid on the next slot. Responds with the applied
///   value.
/// - **`GET /rate`** → the current target cps.
///
/// Runs until the task is cancelled.
pub async fn serve_metrics(
    addr: SocketAddr,
    render: Arc<dyn Fn() -> String + Send + Sync>,
    chaos: Option<Arc<ChaosLog>>,
    rate: Option<RateHandle>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(addr).await?;
    loop {
        let (mut sock, _) = listener.accept().await?;
        let render = render.clone();
        let chaos = chaos.clone();
        let rate = rate.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let (method, path, query) = parse_request_line(&buf[..n]);

            let (status, body) = if method == "POST" && path == "/chaos" {
                let body = if let Some(log) = &chaos {
                    let kind = query_get(&query, "type").unwrap_or_else(|| "unknown".to_string());
                    let target = query_get(&query, "target");
                    // `ts` (Unix epoch ms, captured by the chaos script at the
                    // instant of the kill) back-dates the marker so PF latency does
                    // not shift it; without it we fall back to the receipt instant.
                    match query_get(&query, "ts").and_then(|s| s.parse::<u64>().ok()) {
                        Some(ts) => {
                            log.record_at(kind.clone(), target.clone(), ts);
                            format!("ok: recorded chaos marker type={kind} target={target:?} ts={ts}\n")
                        }
                        None => {
                            log.record(kind.clone(), target.clone());
                            format!("ok: recorded chaos marker type={kind} target={target:?}\n")
                        }
                    }
                } else {
                    "ok: no chaos log attached\n".to_string()
                };
                ("200 OK", body)
            } else if path == "/rate" {
                match &rate {
                    None => ("404 Not Found", "no rate handle attached\n".to_string()),
                    Some(h) if method == "POST" => match query_get(&query, "cps")
                        .map(|s| s.parse::<f64>())
                    {
                        Some(Ok(cps)) => {
                            let applied = h.set(cps);
                            ("200 OK", format!("ok: target cps={applied}\n"))
                        }
                        // A missing or malformed `cps` is a client error (never a
                        // silent no-op that leaves the rate wherever it was).
                        _ => (
                            "400 Bad Request",
                            "expected POST /rate?cps=<float>\n".to_string(),
                        ),
                    },
                    // GET /rate → the current target.
                    Some(h) => ("200 OK", format!("{}\n", h.cps())),
                }
            } else {
                ("200 OK", render())
            };

            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

/// Parse the first request line into `(METHOD, path, query)`. Best-effort: a
/// malformed/empty request yields `("", "", "")` and falls through to the render
/// path. The query is the raw `a=b&c=d` after `?` (empty if none).
fn parse_request_line(buf: &[u8]) -> (String, String, String) {
    let text = String::from_utf8_lossy(buf);
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    (method, path, query)
}

/// Pull `key`'s value out of a raw `a=b&c=d` query string (no percent-decoding —
/// the chaos flags carry simple alnum/`-` values: `kill_worker`, `b2bua-worker-1`).
fn query_get(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key && !v.is_empty()).then(|| v.to_string())
    })
}
