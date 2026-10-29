//! The load driver: a CPS governor that spawns one `Send` task per call onto a
//! shared multi-threaded runtime, bounded by a max-in-flight semaphore, picking
//! scenarios by weighted random. Each per-call task takes its correlation key
//! (minted, or the resolved caller's From user under from-user correlation),
//! binds its agents on the **mux** (one socket per defined endpoint address,
//! many dialogs demuxed), runs the scenario inside a `catch_unwind` boundary (a
//! panic is a *counted* failure, never a worker abort), tears the call down
//! (CANCEL/BYE) however it ended, classifies the scenario's verdict, and
//! records it —
//! optionally projecting a sampled callflow (recording layered on the mux). A
//! call whose key is missing or unusable is recorded `rejected` before any
//! datagram; one whose number is held by a concurrent call or cooling after a
//! failed call re-draws from its case's pool, and is `rejected` only when
//! every draw is unavailable.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use scenario_harness::{AgentBinder, EgressPolicy, WaiverScope};
use sip_clock::Clock;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

use crate::class::{CallOutcome, ResultClass};
use crate::ctx::{CallCtx, CallEnv};
use crate::mix_entry::{LoadBody, MixEntry};
use crate::mux::{CallTally, Correlation, MuxCore};
use crate::outcome::{CallWire, OutcomeSink};
use crate::rate::{Governor, RateHandle};
use crate::report::{CallEvidence, RenderedSample, Reporter};
use crate::{call_rtt::CallRtts, call_task::call_task, failed_call};
use scenario_harness::actor::ScenarioVerdict;

mod binding;
mod record;
mod seeds;
pub(crate) use binding::CALLER;
use binding::{bind_agents, bind_refusal, call_routing, from_user_key, KEY_DRAWS};
use record::{phase_annotated_detail, record_unrun};
use seeds::{mint_token, next_seed, panic_msg, xorshift};

/// The mux transport the driver binds calls on.
pub struct MuxTransport {
    pub core: Arc<MuxCore>,
    /// The caller (UAC) endpoint address.
    pub uac_addr: SocketAddr,
    /// The callee (UAS) endpoint address (the SUT routes the b-leg here). Equal
    /// to [`uac_addr`](Self::uac_addr) in the shared-socket layout, where the
    /// SUT returns each new leg to the ip:port that originated the call.
    pub uas_addr: SocketAddr,
    /// The transfer-target (REFER) endpoint address.
    pub refer_addr: SocketAddr,
    /// How the correlation token travels through the SUT.
    pub correlation: Correlation,
    /// Per-recv wall-clock timeout.
    pub recv_timeout: Duration,
    /// The one process-wide clock every per-call binder records on (created once
    /// at startup), so all call timelines sit on a single monotonic-anchored axis.
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
    /// endpoint (`--route-pin-to-uas`).
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
/// overridden by any per-scenario entry). Both default off: an un-tuned run
/// drops and retransmits nothing extra.
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
    /// Optional DETERMINISTIC targeted drop (test knob): discard the nth
    /// distinct inbound request of a given CSeq method, once or permanently —
    /// see [`crate::mux::TargetedDrop`]. Default off.
    pub drop_nth: Option<crate::mux::TargetedDrop>,
}

/// The settings [`Driver::new`] builds a driver from.
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
    /// The final the run declares its SUT refuses a call with under overload:
    /// a non-emergency call refused with it before it connects ends on its
    /// `refused_overload` branch ([`CallEnv::overload_reject`]). `None`
    /// accepts no undeclared refusal.
    pub overload_reject: Option<u16>,
}

/// Offers calls at the live CPS target, one task per call picked from the
/// weighted mix, shedding a call when `max_in_flight` calls are running.
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
    overload_reject: Option<u16>,
    /// Where each placed call's outcome record goes; `None` = no records.
    outcomes: Option<Arc<OutcomeSink>>,
    /// Read-held by each failed call's release tail; the drain write-locks it.
    tails: Arc<RwLock<()>>,
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
        for e in &scenarios {
            let tuned = cfg.tuning.get(e.id).unwrap_or(&cfg.default_tuning);
            reporter.declare_retransmit(e.id, tuned.retransmit);
        }
        Self {
            rate: RateHandle::new(cfg.cps),
            duration: cfg.duration,
            max_in_flight: cfg.max_in_flight,
            seed: cfg.seed.max(1),
            reporter,
            scenarios,
            total_weight,
            sem: Arc::new(Semaphore::new(cfg.max_in_flight)),
            tails: Arc::default(),
            transport,
            call: Arc::new(cfg.call),
            default_tuning: cfg.default_tuning,
            tuning: cfg.tuning,
            overload_reject: cfg.overload_reject,
            outcomes: None,
        }
    }

    /// Resolve the [`CallTuning`] for a scenario: its per-id override if any, else
    /// the global default.
    fn tuning_for(&self, id: &str) -> CallTuning {
        self.tuning.get(id).copied().unwrap_or(self.default_tuning)
    }

    /// Write one outcome record per placed call into `sink`, and count the
    /// calls never placed (shed, or refused before any datagram).
    pub fn with_outcomes(mut self, sink: Arc<OutcomeSink>) -> Self {
        self.outcomes = Some(sink);
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
            if let Some(sink) = &self.outcomes {
                sink.offered(entry.id);
            }
            let permit = match self.sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    self.reporter.inc_shed(entry.id);
                    if let Some(sink) = &self.outcomes {
                        sink.not_placed(entry.id, "shed");
                    }
                    continue;
                }
            };
            let tuning = self.tuning_for(entry.id);
            let (id, reporter, outcomes) = (entry.id, self.reporter.clone(), self.outcomes.clone());
            let call = run_one(
                entry,
                self.reporter.clone(),
                self.transport.clone(),
                self.call.clone(),
                self.seed,
                tuning,
                self.overload_reject,
                self.outcomes.clone(),
                permit,
                self.tails.clone(),
            );
            tokio::spawn(call_task(call, id, reporter, outcomes));
        }

        // Drain: every permit (in-flight calls), then the lock (release tails).
        let _ = self.sem.acquire_many(self.max_in_flight as u32).await;
        let _ = self.tails.write().await;
    }
}

/// One call, start to finish. `Send + 'static`, so it runs on the shared
/// multi-threaded runtime.
#[allow(clippy::too_many_arguments)] // per-call context threaded explicitly (no shared struct)
pub(crate) async fn run_one(
    entry: MixEntry,
    reporter: Arc<Reporter>,
    transport: Arc<MuxTransport>,
    call: Arc<CallConfig>,
    seed_base: u64,
    tuning: CallTuning,
    overload_reject: Option<u16>,
    outcomes: Option<Arc<OutcomeSink>>,
    _permit: OwnedSemaphorePermit,
    tails: Arc<RwLock<()>>,
) {
    let wire = outcomes.as_ref().map(|_| CallWire::new(transport.clock.clone()));
    reporter.inc_inflight();
    let MixEntry { id, body, case, legs, emergency, challenge_responder, weight: _ } = entry;

    let from_user = transport.correlation.is_from_user();
    let record = reporter.should_record(id);
    // Under from-user correlation a drawn number held by a concurrent call or
    // cooling after a failed one is re-drawn from the case's pool, so one
    // number's contention never fails a call a free number could carry.
    let draws = if from_user && case.is_some() { KEY_DRAWS } else { 1 };
    let mut draw = 0;
    let rtts = CallRtts::new(reporter.clone(), id);
    let tally = Arc::new(CallTally::new(CALLER).with_late_sink(reporter.late_sink(id)));
    let (core, dwells, banner, resolved_input, token, net, binder, alice, callee_agents) = loop {
        draw += 1;
        // Resolve THIS call's binding from the attached Test case (pool walk +
        // token expansion): the core From/To/R-URI to fold into the outgoing
        // INVITE, the per-call dwell overrides over the global CallConfig
        // defaults, the sampled-page banner, and the resolved input the case's
        // checks bind `${input.*}` against. No case → all defaults.
        let (core, dwells, banner, resolved_input) = match case.as_ref().map(|c| c.resolve()) {
            Some(r) => (r.core, r.dwells, Some(r.banner), Some(r.input)),
            None => (Default::default(), Default::default(), None, None),
        };
        // The call's key: minted, or under from-user correlation the resolved
        // caller's From user (the SUT keeps it on every leg it originates); a
        // call without one is refused here, before any bind or datagram.
        let token = if from_user {
            match from_user_key(core.from.as_deref()) {
                Ok(key) => key,
                Err(reason) => {
                    let outcome = CallOutcome::Rejected(reason);
                    return record_unrun(&reporter, &outcomes, id, outcome);
                }
            }
        } else {
            mint_token()
        };
        // Simulated packet loss + auto-retransmit both ride the mux transport
        // (the mux dispatcher is the background pump that reacts to inbound
        // datagrams). The loss RNG is seeded off the call seed so a run is
        // reproducible; 0 rate is a no-op, and retransmit off leaves the
        // transport untouched.
        let mux_net = transport
            .core
            .network_tuned(
                call_routing(&transport, legs, &token),
                tuning.drop_rate,
                tuning.retransmit,
                next_seed(seed_base),
                tuning.drop_nth,
            )
            .with_rtt_sink(rtts.sink())
            .with_tally(tally.clone());
        let net = Arc::new(match &wire {
            Some(w) => mux_net.with_wire_tap(w.tap()),
            None => mux_net,
        });
        let binder =
            AgentBinder::mux(net.clone(), transport.clock.clone(), transport.recv_timeout, record);
        binder.seed_ids(next_seed(seed_base));
        if let Some(sink) = &outcomes {
            // The run (and its call-id tag) is the Call-ID nonce: no two share one.
            binder.call_id_nonce(&sink.call_id_nonce);
        }
        match bind_agents(&binder, &transport, legs).await {
            Ok((alice, callees)) => {
                break (core, dwells, banner, resolved_input, token, net, binder, alice, callees)
            }
            Err(e) => match bind_refusal(&e) {
                CallOutcome::Rejected(_) if draw < draws => continue,
                outcome => return record_unrun(&reporter, &outcomes, id, outcome),
            },
        }
    };
    let key = token.clone();
    let agent_for = |role: &str| callee_agents.iter().find(|(r, _)| *r == role).map(|(_, a)| a);
    // The primary callee: the "bob" role (every derived spec — and every downstream
    // shape — declares it), falling back to the first declared leg for an
    // exotic spec that names its primary differently.
    let bob = agent_for("bob").unwrap_or(&callee_agents[0].1);

    // A SAMPLED call collects message anchors (ADR-0019) so the attached Test
    // case's checks can resolve `<agent>.<anchor>` over its recorded trace;
    // on the unsampled majority tagging stays a single atomic load.
    let ctx = CallCtx::new();
    if record {
        ctx.enable_anchor_collection();
    }
    // At the verdict, before the runner's own teardown: seal the wire facts
    // and charge a failure to each lane's last message in.
    let (seal, mark) = (wire.clone(), tally.clone());
    ctx.on_resolved(move || {
        seal.iter().for_each(CallWire::seal);
        mark.mark_outcome();
    });

    let env = CallEnv {
        alice: &alice,
        bob,
        bob2: agent_for("bob2"),
        charlie: agent_for("charlie"),
        // Every named leg, so a load body resolves ANY declared role
        // (`callee_agent("mrf")`), not just bob / bob2 / charlie.
        callees: callee_agents.iter().map(|(role, agent)| (*role, agent)).collect(),
        via: call.via,
        stamp: transport.correlation.stamp(&token),
        token,
        emergency,
        overload_reject,
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
    // The single body-invocation site: an actor scenario rides the per-endpoint
    // reactor runner (it owns the per-actor teardown scopes), yielding the
    // verdict everything downstream buckets on, inside a `catch_unwind`
    // boundary (a panic is a counted failure). The body is built ONCE so its
    // declared waivers (ADR-0024 §6) reach the audit; a build guard error is
    // the call's `build` failure, a build panic caught like a run panic.
    let mut plan_waivers: Vec<WaiverScope> = Vec::new();
    let result = match &body {
        LoadBody::Actor(scenario) => {
            match std::panic::catch_unwind(AssertUnwindSafe(|| scenario.build(&env))) {
                Ok(Ok(call)) => {
                    plan_waivers = call.waivers.clone();
                    AssertUnwindSafe(scenario_harness::actor::run_built_actor_call(
                        call, &env, &ctx,
                    ))
                    .catch_unwind()
                    .await
                }
                Ok(Err(e)) => Ok(ScenarioVerdict::build_failure(e)),
                Err(payload) => Err(payload),
            }
        }
    };

    tally.mark_outcome(); // a call whose runner never resolved (its build failed)
    let failed = !matches!(result, Ok(ScenarioVerdict::Success { .. }));
    let (fold_tx, fold_rx) = tokio::sync::oneshot::channel();
    if failed {
        let quiesce = call.teardown_quiesce;
        failed_call::release(tails, net, &alice, &callee_agents, quiesce, fold_rx).await;
    }

    // The per-call audit policy (ADR-0024 §6): the merged structural waivers —
    // the attached case's lowered `allowViolations` PLUS the plan's declared
    // scopes — filter the findings through the `WaiverScope` path (party
    // attribution over each finding's offending mux sub-lane). Stable per
    // scenario, so the per-CAMPAIGN unused-waiver tally aligns.
    let mut merged_waivers: Vec<WaiverScope> =
        case.as_ref().map(|c| c.waivers().to_vec()).unwrap_or_default();
    merged_waivers.extend(plan_waivers);

    let outcome = match result {
        Ok(verdict) => CallOutcome::from(verdict),
        Err(payload) => CallOutcome::Panic(panic_msg(payload)),
    };
    let accepted = outcome.is_checkable();
    let outcome = outcome.audited(|| {
        let wo = binder.rfc_findings(&merged_waivers);
        // Fold this sampled call's per-waiver use into the CAMPAIGN tally
        // (never per call — a call exercises only its own branch).
        reporter.record_waiver_use(id, &merged_waivers, &wo.used);
        wo.findings
    });

    // Test-case CHECKS — evaluated on SAMPLED calls that ended on an accepted
    // branch only (the per-sample oracle; a non-sampled call has
    // no recording to check, and a failed/RFC-dirty call already explains
    // itself). The verdicts (pass AND
    // fail) render on the sampled callflow page; any failed check reclassifies
    // the call to `check_fail`.
    let verdicts: Vec<e2e_model::CheckVerdict> = match (&outcome, case.as_ref(), &resolved_input) {
        (o, Some(c), Some(input)) if o.is_checkable() && record && c.has_checks() => {
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
        CallOutcome::CheckFail(verdicts.iter().filter(|v| !v.passed).cloned().collect())
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
    // A from-user key whose call ended not ok cools before another call may
    // draw it; released while this call's endpoints still hold it.
    if transport.correlation.is_from_user() {
        transport.core.release_key(&key, class.is_ok());
    }
    let e2e = ctx.elapsed();
    let checkpoints = ctx.take_checkpoints();
    // Fold this call's 18x outcome into the cross-call ringing-delivery gate (a
    // dropped non-PRACK 18x is expected, so it is a rate, not a per-call failure).
    reporter.record_ringing(ctx.ringing());

    // The bounded case discriminator (which RFC rule / failed check / failed
    // step) — computed once; keys the first-N sample buckets AND the counts
    // split.
    let case = outcome.case(ctx.phases().last().map(|(name, _)| *name));

    // Only a recorded call is a sample: an unrecorded one never takes a slot
    // a later recorded call of its bucket would fill with a full callflow.
    let sample = if binder.is_recording() && reporter.wants_sample(id, &class, &case) {
        let detail = phase_annotated_detail(outcome.detail(), &ctx);
        // A sampled NOK page states WHY (banner + anomaly); the banner (the
        // resolved binding) shows on PASS and FAIL alike, the failure detail on
        // FAIL only, the case's check verdicts pass AND fail.
        binder
            .render_html(id, class.is_ok(), banner.as_deref(), detail.as_deref(), &check_notes)
            .map(|html| RenderedSample {
                html: Some(Arc::from(html)),
                detail,
                e2e_ms: e2e.as_secs_f64() * 1000.0,
            })
    } else {
        None
    };

    let evidence = CallEvidence { recorded: binder.is_recording(), accepted, sample };
    reporter.record(id, &outcome, &case, e2e, &checkpoints, evidence);
    // The ladder takes the call's messages once its traffic is over.
    let fold = record::ladder_fold(&reporter, id, &tally, &outcome);
    match failed {
        true => drop(fold_tx.send(fold)),
        false => fold(),
    }
    rtts.finish();
    if let (Some(sink), Some(wire)) = (&outcomes, &wire) {
        crate::outcome::write_call(sink, id, &wire.facts(), &outcome, transport.clock.now_ms());
    }
    reporter.dec_inflight();
}
