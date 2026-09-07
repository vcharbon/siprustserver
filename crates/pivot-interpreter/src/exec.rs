//! The **executor**: a plan instance driven over the scenario harness.
//!
//! The whole job (`PCAP2TEST_PIVOT_V3.md` §14) and nothing beside it: bind the
//! endpoints the lane supplies, sequence the flow through [`Cursor`], emit what
//! a `send` states through [`LegStack`], gate an `expect` through [`gate`],
//! answer `background` without moving the cursor, record every datagram
//! verbatim, settle, and evaluate `postconditions`.
//!
//! **A datagram that does not match is a failure.** The agents drop to the raw
//! wire so nothing is absorbed beneath the interpreter, and the ONE §17.2
//! implementation ([`Absorption`]) runs HERE — after the datagram is recorded.
//! A byte-identical retransmission the transaction layer owns is therefore in
//! the bundle, noted as absorbed, and never reaches an expect; one the TU owns
//! (a 2xx to an INVITE, an ACK to a 2xx — see the two-view table in
//! `scenario_harness::absorption`) surfaces like any other datagram.
//! Everything that surfaces is matched, answered by a `background` policy, or
//! recorded as the failure it is: no absorb-set, no window, no tolerance list.
//!
//! **A failure does not by itself end the run** (§11.2). What ends the SCRIPT is
//! a failure the run cannot go on past — a required expect that timed out, or an
//! arrival that left an armed expect unsatisfiable ([`crate::progress`]) — and
//! then [`crate::close`] terminates what the scripted legs still hold and the
//! run settles like any other. Any other failure is recorded and the flow
//! continues, so the verdict sees the whole divergence. This is polarity-free: a
//! declaration (§11.2) changes nothing about it, and what a failure MEANS is the
//! verdict's ([`crate::must_fail`]), decided once the run is over.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use pivot_schema::bundle::{
    Abandoned, CloseAct, Dir, Failure, LadderSide, RunConfig, RunTiming, RunVerdict, TimingNote,
};
use pivot_schema::deviation::CseqValue;
use pivot_schema::flow::Anchor;
use scenario_harness::absorption::{
    ack_key_of_ack, ack_key_of_final, Absorption, AckKey, Owner, SeenBy, WireEntry,
};
use scenario_harness::{Agent, Inbound as HarnessInbound};
use sip_message::{Method, SipMessage, SipResponse};
use sip_retransmit::Schedule;
use tokio::time::Instant;

use crate::checks::{self, MessageObservables};
use crate::close::{self, Owed};
use crate::deviation::StepEffects;
use crate::early::{EarlyDialogs, LearnedForks};
use crate::gate::{self, GateVerdict, Inbound};
use crate::instance::Instance;
use crate::plan::{CompiledStep, Discriminator, Plan};
use crate::preserve;
use crate::progress;
use crate::recording::Recording;
use crate::render::{self, UriComposer};
use crate::resolve::Resolver;
use crate::retransmit::{self, DrawnAck, DrawnAcks, Repeats};
use crate::scope::Finding;
use crate::settle::{self, Sut};
use crate::stack::LegStack;
use crate::state::StepOutcome;

/// The site an emission the FLOW never scripted is attributed to: it belongs to
/// no step, because no step is what it answers.
const UNSCRIPTED: &str = "(unscripted)";

/// The recording note for the RFC 3261 §15.1.2 final the document never held.
const OWED_BYE_FINAL: &str =
    "absorbed: the §15.1.2 final owed to the BYE this leg sent, which no expect scripts";

/// Whether the step is the document's own answer to a BYE — a final gated on a
/// BYE transaction, whatever status it names.
fn scripts_a_bye_final(step: &CompiledStep) -> bool {
    matches!(
        &step.discriminator,
        Discriminator::Response { status, cseq_method: Some(method) }
            if *status >= 200 && method.eq_ignore_ascii_case("BYE")
    )
}

/// How long a step holds a datagram out for a better candidate.
///
/// A BACKSTOP, not a wait: the hold normally ends on the next arrival, and this
/// is only reached when the leg goes silent — which is itself the answer, that
/// nothing better was coming. So it is sized to outlast one relay hop on any
/// lane the interpreter drives (a simulated fabric's 100 ms, a real lane's
/// single-digit milliseconds) while staying far inside the smallest expect
/// budget a document states, since a step that reaches the backstop completes
/// this much later than it otherwise would.
const HOLD_OUT_MS: u64 = 500;

/// The recording note a held datagram carries while the hold-out is open.
const HELD_NOTE: &str =
    "held: the armed expect froze headers this datagram carries none of, so it waits to see \
     whether a better candidate for that step lands in the same instant";

/// The note it carries once the hold-out closed with nothing better.
const HELD_TAKEN: &str = "held out for a better candidate; none landed, so the step took this one";

/// What the lane supplies: a bound agent per actor, where a request goes, and
/// how a tier-2 position becomes a URI.
pub struct Lane<'a> {
    /// Actor id → the agent that plays it.
    pub agents: BTreeMap<String, Agent>,
    /// The system under test's ingress: where an out-of-dialog request goes.
    pub route_target: std::net::SocketAddr,
    /// The lane's media plane: the address an SDP body's connection line is
    /// rewritten to and the port book its media lines draw from.
    pub media: crate::media::Booking,
    /// Where a body resource reference resolves from.
    pub base_dir: PathBuf,
    /// How a tier-2 position becomes a URI on this lane.
    pub composer: &'a dyn UriComposer,
}

/// One finished run.
pub struct Outcome {
    pub verdict: RunVerdict,
    pub recording: Recording,
    pub timing: RunTiming,
    /// Every datagram the run sighted, in arrival order, each tagged with the
    /// view it belongs to — the WIRE view of issue 22's pair. The recording is
    /// this same stream rendered per leg.
    pub wire_view: Vec<WireEntry>,
}

impl Outcome {
    /// What the transaction user saw: the `SeenBy::Both` subset of
    /// [`wire_view`](Self::wire_view), in the same order.
    pub fn tu_view(&self) -> Vec<WireEntry> {
        self.wire_view.iter().filter(|e| e.seen_by() == SeenBy::Both).cloned().collect()
    }
}

/// Run `plan` on `lane` against `sut`, recording into a handle the caller
/// already holds — what a [`BundleWriter`](crate::bundle::BundleWriter) armed
/// before the run body needs so an unwinding run still leaves its ladder. The
/// recording survives whatever the loop decides: it is returned in the
/// [`Outcome`] beside the verdict.
pub async fn run_recording_into(
    plan: &Plan,
    config: RunConfig,
    lane: Lane<'_>,
    sut: &dyn Sut,
    recording: Recording,
) -> Outcome {
    let mut instance = Instance::with_recording(plan, config, recording.clone());
    let (timing, wire_view) = if instance.verdict().passed() {
        let mut runner = Runner::new(&mut instance, lane);
        let timing = runner.drive(sut).await;
        let wire_view = runner.absorption.wire_view();
        (timing, wire_view)
    } else {
        // The lane's configuration is already refused (an unbound identity):
        // dialling anyway would report an unclaimed arrival instead of the cause.
        (
            RunTiming {
                started_at_ms: 0,
                settled_at_ms: None,
                settle_budget_ms: plan.document().timing.settle_budget_ms,
            },
            Vec::new(),
        )
    };
    instance.seal_verdict();
    Outcome { verdict: instance.verdict().clone(), recording, timing, wire_view }
}

/// The run loop's own state, kept beside the instance it drives.
struct Runner<'a, 'p> {
    instance: &'a mut Instance<'p>,
    lane: Lane<'a>,
    stacks: BTreeMap<String, LegStack>,
    /// Step id → when it completed, for a dwell anchored on it.
    completed_at: BTreeMap<String, Instant>,
    /// Step id → when its budget opens; a dwell may put that in the future.
    armed_at: BTreeMap<String, Instant>,
    /// Leg id → the INVITE finals it received, in arrival order, one per
    /// transaction: the pool an auto ACK step draws the final it acknowledges
    /// from.
    finals: BTreeMap<String, Vec<SipResponse>>,
    /// The ANSWERED forks the document names, minted before the run speaks.
    early: EarlyDialogs,
    /// The OBSERVED forks this run has learned, bound on consumption (§6.1).
    learned: LearnedForks,
    /// The §17.2 receive view, and the two views it separates. It runs HERE so
    /// an absorbed repeat is recorded before it is dropped.
    absorption: Absorption,
    /// Each step's declared `retransmits` ladder, against what the run saw
    /// (§6.9).
    repeats: Repeats,
    /// The ACK ladders the wire draws rather than a timer paces (§6.3).
    drawn: DrawnAcks,
    /// Whether the settle window has already reported a late arrival.
    late_datagram_reported: bool,
    /// Where a NEGATIVE document's first aborting delta ended the script
    /// (§11.2), and what the generic close then emitted. `None` on every run
    /// that followed its flow.
    abandoned: Option<Abandoned>,
    /// Legs whose close emission was refused. A close that cannot compose or
    /// send says so ONCE: retrying it every settle turn would bury the cause
    /// under a thousand copies of itself.
    close_refused: std::collections::BTreeSet<String>,
    /// `(leg, CSeq)` of every BYE the script SENT: RFC 3261 §15.1.2 owes each
    /// one a final response, whatever the document scripts for it.
    byes_sent: std::collections::BTreeSet<(String, u32)>,
    /// The ladder rungs still owed, each with the instant it is due. A ladder
    /// is a peer's own transaction timer and gates nothing: it waits BESIDE the
    /// loop, never inside it, so every other leg keeps its own clock while a
    /// ladder runs (§6.9).
    pending_repeats: Vec<PendingRepeat>,
    /// The one datagram a step is holding out on, and when it gives up. At most
    /// one at a time: the hold arbitrates a single instant, and a second hold
    /// would be a queue.
    held: Option<Held>,
    started: Instant,
}

/// A datagram an armed expect is holding out on, recorded already, undelivered.
struct Held {
    actor: String,
    leg: String,
    /// The step that holds it — what a better candidate has to satisfy.
    step: String,
    inbound: Inbound,
    message: SipMessage,
    raw: String,
    bytes: Vec<u8>,
    repeat: bool,
    /// Its sequence number in the recording, so releasing it re-notes the entry
    /// it already has instead of writing a second one.
    seq: Option<u64>,
    release_at: Instant,
}

/// One ladder rung, waiting for the instant it is due.
struct PendingRepeat {
    step: String,
    leg: String,
    agent: Agent,
    wire: Vec<u8>,
    dst: std::net::SocketAddr,
    at: Instant,
    n: u32,
    count: u32,
}

impl<'a, 'p> Runner<'a, 'p> {
    fn new(instance: &'a mut Instance<'p>, lane: Lane<'a>) -> Self {
        let started = Instant::now();
        // The interpreter owns §17.2 (see [`Absorption`]): the agents must not
        // absorb beneath it, or the retransmissions never reach the recording.
        for agent in lane.agents.values() {
            agent.drop_to_raw_wire();
        }
        let nonce = instance.nonce().to_string();
        let mut stacks = BTreeMap::new();
        for (id, leg) in instance.plan().legs() {
            let actor = leg.actor.clone();
            // "Bound and heard nothing" and "never got that far" are different
            // findings, so a bound leg gets its ladder before it speaks.
            if lane.agents.contains_key(&actor) {
                instance.recording().declare(id);
            }
            let addr =
                lane.agents.get(&actor).map(Agent::addr).unwrap_or_else(|| lane.route_target);
            stacks.insert(
                id.clone(),
                LegStack::new(
                    id.clone(),
                    addr,
                    lane.route_target,
                    &nonce,
                    String::new(),
                    String::new(),
                ),
            );
        }
        let early = EarlyDialogs::mint(instance.plan().steps(), &nonce);
        // An ANSWERED fork's tag is minted before the run speaks, so
        // `${early:….tag}` answers from the first step onwards. An OBSERVED
        // fork's is published the moment `deliver` learns it.
        for (leg, id, tag) in early.iter() {
            instance.mint_early(id, leg, tag);
        }
        Runner {
            instance,
            lane,
            early,
            learned: LearnedForks::default(),
            stacks,
            completed_at: BTreeMap::new(),
            armed_at: BTreeMap::new(),
            finals: BTreeMap::new(),
            absorption: Absorption::transaction_view(),
            repeats: Repeats::new(),
            drawn: DrawnAcks::new(),
            late_datagram_reported: false,
            abandoned: None,
            close_refused: std::collections::BTreeSet::new(),
            byes_sent: std::collections::BTreeSet::new(),
            pending_repeats: Vec::new(),
            held: None,
            started,
        }
    }

    fn now_us(&self) -> u64 {
        Instant::now().duration_since(self.started).as_micros() as u64
    }

    fn now_ms(&self) -> u64 {
        Instant::now().duration_since(self.started).as_millis() as u64
    }

    /// Wall time one loop of this run may burn before it is declared stuck: the
    /// lane's clock read against the timeline its document declares.
    fn wall_ceiling(&self) -> Duration {
        let document = self.instance.plan().document();
        Duration::from_millis(
            self.instance
                .config()
                .wall_ceiling_ms(document.declared_span_ms(), document.timing.settle_budget_ms),
        )
    }

    /// Drive the flow, then settle. Returns the run's timing whatever happened.
    async fn drive(&mut self, sut: &dyn Sut) -> RunTiming {
        let settle_budget = self.instance.plan().document().timing.settle_budget_ms;
        let flow_ok = self.run_flow().await;
        // Whatever the flow did, nothing is still HELD when the settle begins:
        // a hold outliving the flow is an unanswered request in front of a SUT
        // that will eventually tear the call down over it, which would put a
        // second cause on a run that already has one.
        let flow_ok = self.concede_held().await && flow_ok;
        if flow_ok && !self.instance.cursor().is_done() {
            let pending = self.instance.cursor().pending();
            self.instance.fail(Failure::FlowIncomplete { pending });
        }
        // A run refused BEFORE anything reached the wire has nothing to settle:
        // no dialog was opened, so waiting out the budget adds a second failure
        // that says less than the first. A run that DID speak always settles,
        // failure or not — that is where the evidence is.
        let settled_at = if !flow_ok && self.instance.recording().is_empty() {
            None
        } else {
            self.settle(sut, settle_budget, flow_ok).await
        };
        // AFTER settle: a ladder's last repeats legitimately land in the settle
        // window, so counting before it would report a short ladder that is
        // still arriving.
        self.reconcile_retransmits();
        self.state_abandonment();
        RunTiming {
            started_at_ms: 0,
            settled_at_ms: settled_at,
            settle_budget_ms: settle_budget,
        }
    }

    /// The flow loop. `false` once a failure aborts it.
    async fn run_flow(&mut self) -> bool {
        // A run must never hang. Every turn of this loop either moves the flow
        // or advances the clock; if neither happens for a long stretch the loop
        // cannot make progress, and saying so beats spinning forever.
        let mut stall = Stall::new(self.wall_ceiling());
        loop {
            if let Some(failure) = stall.check("the flow", || {
                self.instance.cursor().frontier()
            }) {
                self.instance.fail(failure);
                self.end_script(None, None);
                return false;
            }
            if !self.refuse_injects() {
                return false;
            }
            // A rung due goes out BEFORE the turn's steps: its instant was
            // booked first, and the peer paces its own answer off it.
            if !self.drain_repeats().await {
                return false;
            }
            if !self.emit_ready().await {
                return false;
            }
            if self.instance.cursor().is_done() {
                // A hold-out the flow outlived belongs to the policy: no step is
                // left to take it, and the settle must not meet an unanswered
                // request the run is still carrying.
                return self.concede_held().await;
            }
            self.arm_expects();
            let Some(wake) = self.next_wake() else {
                // Nothing to send and nothing to wait for, yet the flow is not
                // done: the run cannot progress and says so.
                let pending = self.instance.cursor().pending();
                self.instance.fail(Failure::FlowIncomplete { pending });
                self.end_script(None, None);
                return false;
            };
            match self.wait(wake).await {
                Wake::Timer => {
                    // The hold-out closes BEFORE any budget does: the datagram
                    // it holds is what an armed expect on this leg was waiting
                    // for, and expiring that expect first would fail the run on
                    // a message the run is still carrying.
                    if self.held.as_ref().is_some_and(|h| h.release_at <= Instant::now())
                        && !self.release_held().await
                    {
                        return false;
                    }
                    if !self.expire_expects() {
                        return false;
                    }
                }
                Wake::Message { actor, message } => {
                    if !self.dispatch(&actor, *message).await {
                        return false;
                    }
                }
                Wake::Closed { actor, detail } => {
                    self.instance.fail(Failure::TransportClosed { actor, detail });
                    // The close still runs: only THIS actor's transport is gone,
                    // and what another leg holds open is still endable.
                    self.end_script(None, None);
                    return false;
                }
            }
        }
    }

    /// An `inject` the lane cannot perform stops the run by name. `inject` is
    /// shape-only in this program: no interpreter executes an action.
    fn refuse_injects(&mut self) -> bool {
        let open = self.instance.cursor().open_injects();
        if open.is_empty() {
            return true;
        }
        let first = open.first().map(|(node, _)| node.to_string());
        for (node, action) in open {
            self.instance
                .fail(Failure::InjectorMissing { node: node.to_string(), action: action.to_string() });
        }
        let leg = first
            .as_deref()
            .and_then(|node| self.instance.plan().step(node).map(|s| s.leg.clone()));
        self.end_script(leg.as_deref(), first.as_deref());
        false
    }

    /// Emit every send whose leg is at it, whose `after` holds and whose dwell
    /// has elapsed.
    async fn emit_ready(&mut self) -> bool {
        loop {
            let Some(step) = self.next_send() else { return true };
            if !self.emit(&step).await {
                // A send the run cannot compose, pace or put on the wire is a
                // script it cannot keep following (§11.2): the close ends what
                // the legs still hold.
                self.end_script(Some(&step.leg), Some(&step.id));
                return false;
            }
        }
    }

    /// The next send that is ready NOW.
    fn next_send(&self) -> Option<CompiledStep> {
        let now = Instant::now();
        self.instance
            .cursor()
            .frontier()
            .iter()
            .filter_map(|id| self.instance.plan().step(id))
            .find(|step| {
                step.is_send() && self.dwell_deadline(step).is_some_and(|at| at <= now)
            })
            .cloned()
    }

    /// When a `delay`'s anchor happened, or `None` while it has not.
    ///
    /// A duration measured from a node that has not happened is not a duration,
    /// and defaulting it to the run's start would place it before the message it
    /// is anchored on. An anchor that is RELEASED (a tolerated absence) settles
    /// at its release, and a BLOCK settles when the block completes, so neither
    /// an `optional` nor an `alt` anchor wedges what is anchored on it.
    fn anchor_at(&self, anchor: &Anchor) -> Option<Instant> {
        match anchor {
            Anchor::Trigger => Some(self.started),
            Anchor::Step(id) => self.completed_at.get(id).copied(),
        }
    }

    /// When a step's dwell expires: its anchor's own time plus `delay.ms`.
    ///
    /// On a SEND this is when the message goes out; on an EXPECT it is when the
    /// budget opens (see [`Self::arm_expects`]).
    ///
    /// A compressible dwell on a virtual clock still rides — virtual time costs
    /// nothing and the dwell carries the causal order — while a `timer_linked`
    /// one is what a system timer measures and is never shortened.
    fn dwell_deadline(&self, step: &CompiledStep) -> Option<Instant> {
        Some(self.anchor_at(&step.delay.from)? + Duration::from_millis(step.delay.ms))
    }

    /// Open the budget of every frontier expect whose dwell is known and whose
    /// budget is not open yet.
    ///
    /// It opens at the LATER of the leg reaching the step and the step's own
    /// dwell — the very `anchor + delay.ms` a send sleeps. The dwell gates when
    /// the budget STARTS, never the match: an expect whose budget has not opened
    /// is as matchable as one whose has, it just cannot expire. Opening on the
    /// frontier alone would spend the budget across the dwell itself, failing a
    /// faithful run for a message the flow has not yet been asked to provoke.
    fn arm_expects(&mut self) {
        let now = Instant::now();
        let opening: Vec<(String, Instant)> = self
            .instance
            .cursor()
            .frontier()
            .into_iter()
            .filter_map(|id| {
                let step = self.instance.plan().step(&id)?;
                if !step.is_expect() {
                    return None;
                }
                let opens = now.max(self.dwell_deadline(step)?);
                Some((id, opens))
            })
            .collect();
        for (id, opens) in opening {
            self.armed_at.entry(id).or_insert(opens);
        }
    }

    /// The next moment the loop must wake: a dwell, or an expect's budget.
    ///
    /// A step whose anchor has not settled has no deadline yet — for either op,
    /// what wakes the run is the anchor's own arrival.
    fn next_wake(&self) -> Option<Instant> {
        let mut soonest: Option<Instant> = None;
        for id in self.instance.cursor().frontier() {
            let Some(step) = self.instance.plan().step(&id) else { continue };
            let at = if step.is_send() {
                match self.dwell_deadline(step) {
                    Some(at) => at,
                    None => continue,
                }
            } else {
                match self.armed_at.get(&id) {
                    Some(opened) => *opened + Duration::from_millis(step.within_ms),
                    None => continue,
                }
            };
            soonest = Some(soonest.map_or(at, |s: Instant| s.min(at)));
        }
        // A rung owed is a wake owed: the flow may have nothing else pending
        // and the ladder must still go out on time.
        if let Some(at) = self.next_repeat() {
            soonest = Some(soonest.map_or(at, |s: Instant| s.min(at)));
        }
        // A hold-out that nothing better closed must still close: the datagram
        // it holds is one an armed expect would otherwise have taken.
        if let Some(at) = self.held.as_ref().map(|h| h.release_at) {
            soonest = Some(soonest.map_or(at, |s: Instant| s.min(at)));
        }
        soonest
    }

    /// Wait for the first of: a datagram on any bound socket, or `wake`.
    async fn wait(&mut self, wake: Instant) -> Wake {
        let mut pumps = FuturesUnordered::new();
        // One receive future per distinct UA stack: two actors on one socket
        // share a queue, and pumping it twice would race the same datagram.
        let mut seen = std::collections::BTreeSet::new();
        for (actor, agent) in &self.lane.agents {
            if !seen.insert(agent.stack_id()) {
                continue;
            }
            let actor = actor.clone();
            let agent = agent.clone();
            pumps.push(async move {
                let inbound = agent.recv_any().await;
                (actor, inbound)
            });
        }
        tokio::select! {
            biased;
            Some((actor, inbound)) = pumps.next() => match inbound {
                Ok(HarnessInbound::Request(txn)) => Wake::Message {
                    actor,
                    message: Box::new(SipMessage::Request(txn.request().clone())),
                },
                Ok(HarnessInbound::Response(response)) => Wake::Message {
                    actor,
                    message: Box::new(SipMessage::Response(response)),
                },
                // A per-recv timeout is "nothing arrived yet", not an error: the
                // loop's own deadlines decide when waiting has gone on too long.
                Err(scenario_harness::StepError::Timeout { .. }) => Wake::Timer,
                Err(e) => Wake::Closed { actor, detail: e.to_string() },
            },
            () = tokio::time::sleep_until(wake) => Wake::Timer,
        }
    }

    /// A budget that ran out. A required expect FAILS and ENDS THE SCRIPT — the
    /// message it waits for is missing, so the run cannot compose what comes
    /// after it (§11.2); an `optional` one is released, because the budget is how
    /// long the document tolerates the absence it already declared — and a
    /// release is the only rule that does not wedge the steps behind it forever.
    fn expire_expects(&mut self) -> bool {
        let now = Instant::now();
        let expired: Vec<CompiledStep> = self
            .instance
            .cursor()
            .frontier()
            .iter()
            .filter_map(|id| self.instance.plan().step(id).map(|s| (id.clone(), s)))
            .filter(|(id, step)| {
                step.is_expect()
                    && self
                        .armed_at
                        .get(id)
                        .is_some_and(|opened| {
                            now >= *opened + Duration::from_millis(step.within_ms)
                        })
            })
            .map(|(_, step)| step.clone())
            .collect();
        let mut failed = false;
        for step in expired {
            if step.optional_expect() {
                if self.instance.release_step(&step.id) {
                    self.completed_at.insert(step.id.clone(), now);
                    self.settle_blocks(now);
                }
                continue;
            }
            self.instance.fail(Failure::ExpectTimedOut {
                step: step.id.clone(),
                leg: step.leg.clone(),
                gated_on: step.discriminator.to_string(),
                within_ms: step.within_ms,
            });
            self.end_script(Some(&step.leg), Some(&step.id));
            failed = true;
        }
        !failed
    }

    /// Record, absorb-or-surface, attribute and dispatch one arriving datagram.
    async fn dispatch(&mut self, actor: &str, message: SipMessage) -> bool {
        let inbound = Inbound::of(&message);
        let bytes: Vec<u8> = match &message {
            SipMessage::Request(r) => r.image().to_vec(),
            SipMessage::Response(r) => r.image().to_vec(),
        };
        let raw = String::from_utf8_lossy(&bytes).into_owned();

        // §17.2 FIRST, and before any claim: a retransmitted INVITE must not
        // consume a second claim on its way to being absorbed. The datagram is
        // still recorded — that is why the rule runs here and not below the
        // agent's own API.
        let sighting = self.absorption.sight(&bytes, &message);
        let repeat = sighting.repeat;
        if sighting.owner == Owner::TxnDuplicate {
            let leg = self
                .leg_by_call_id(&inbound.call_id)
                .unwrap_or_else(|| "unattributed".to_string());
            self.repeats.note(&leg, &bytes, self.now_us());
            self.record_arrival(
                &leg,
                raw,
                None,
                Some("absorbed: byte-identical retransmission of a datagram already surfaced"),
                repeat,
            );
            return true;
        }

        // A repeat the transaction layer does NOT absorb — a 2xx to an INVITE,
        // an ACK, a replayed provisional — still satisfies no second expect: the
        // step it repeats has already matched and the flow has moved on. Where
        // the document DECLARES the ladder (§6.9) it is counted and recorded;
        // where it does not, it falls through to the gate and fails there, which
        // is what an unannounced repeat is.
        if let Some(leg) = self.leg_by_call_id(&inbound.call_id) {
            if let Some(step) = self.repeats.note(&leg, &bytes, self.now_us()) {
                // A DECLARED ladder's repeat, always recorded as one: §6.9 counts
                // the repeats of a step's own message, which is not the same unit
                // as §17.2's duplicate — a repeated provisional is deliberately
                // never a duplicate, and it is still this step's retransmission.
                let note = format!("retransmission of the datagram step {step:?} matched");
                self.instance.recording().push_repeat(
                    &leg,
                    Dir::In,
                    self.now_us(),
                    raw,
                    None,
                    Some(&note),
                );
                // A repeated 2xx draws its own ACK (§6.3): the seam has already
                // ruled it visible to the transaction user, so the UAC core owes
                // one more copy of the ACK it sent.
                if !self.draw_for_repeat(&message).await {
                    self.end_script(Some(&leg), None);
                    return false;
                }
                return true;
            }
        }

        // Background next (§5.1): a policy's traffic never touches the cursor.
        // The policy is sought over every actor on the RECEIVING endpoint, not
        // only the one whose pump surfaced it: a loopback endpoint hosts a UAC
        // and a UAS on one socket, and only one of them is pumped.
        let known_leg = self.leg_by_call_id(&inbound.call_id);
        let awaited = known_leg.as_deref().is_some_and(|leg| self.flow_awaits(leg, &inbound));
        if let Some(method) = &inbound.method {
            if let Some((index, policy)) = (!awaited)
                .then(|| self.background_policy(actor, method, known_leg.as_deref()))
                .flatten()
            {
                let leg = known_leg.clone().unwrap_or_else(|| policy.actor.clone());
                self.record_arrival(&leg, raw, None, Some("background policy"), repeat);
                self.instance.note_background_answered(index);
                if !self.answer_background(actor, &leg, &message, policy.status).await {
                    self.end_script(Some(&leg), None);
                    return false;
                }
                return true;
            }
        }

        // §5.1 arbitration, where the window is not enough to tell the two
        // apart: a step and a policy can BOTH want this method at this instant,
        // and arrival order is not evidence of which one it is. A step that
        // froze headers and got a datagram carrying none of them holds out for
        // one that carries them, and gives up the moment nothing better lands.
        if let Some(step) = self.holds_out_for_better(known_leg.as_deref(), actor, &inbound) {
            let leg = known_leg.clone().unwrap_or_default();
            self.record_arrival(&leg, raw.clone(), None, Some(HELD_NOTE), repeat);
            let seq = self.instance.recording().last_seq(&leg);
            self.held = Some(Held {
                actor: actor.to_string(),
                leg,
                step,
                inbound,
                message,
                raw,
                bytes,
                repeat,
                seq,
                release_at: Instant::now() + Duration::from_millis(HOLD_OUT_MS),
            });
            return true;
        }

        self.deliver(actor, inbound, message, raw, bytes, repeat, None).await
    }

    /// The fork gate for `step`, under whichever §6.1 reading the plan gave its
    /// id: an ANSWERED fork gates on the tag this run minted, an OBSERVED one
    /// on what the leg has learned so far. Pure either way — binding is
    /// `deliver`'s, on consumption.
    fn rides_fork(&self, step: &CompiledStep, inbound: &Inbound) -> GateVerdict {
        match self.early.tag_of(step) {
            Some(tag) => gate::rides_early_dialog(step, inbound, Some(tag)),
            None => gate::rides_observed_fork(step, inbound, &self.learned),
        }
    }

    /// The To-tag of the fork a step names, under whichever §6.1 reading the
    /// plan gave its id: the tag this run MINTED for an ANSWERED fork, the tag
    /// the leg LEARNED for an OBSERVED one — absent there until an arrival
    /// taught it.
    fn fork_tag_of(&self, step: &CompiledStep) -> Option<&str> {
        self.early
            .tag_of(step)
            .or_else(|| step.early.as_deref().and_then(|early| self.learned.tag(&step.leg, early)))
    }

    /// Whether `step` may take a datagram whose frozen HEADER SET did not hold.
    ///
    /// Arbitration, never tolerance: the relaxed reading stands unless something
    /// ELSE on this leg could be the datagram's owner. Only a `background`
    /// policy can be — a policy answers a METHOD outside the flow (§5.1) and
    /// never a response — and the one thing on the wire that tells the two
    /// readings apart is whether the datagram carries any of the vocabulary the
    /// step froze ([`gate::declared_headers_carried`]). Nothing contests a
    /// datagram whose discriminator, leg and fork are already the step's.
    fn takes_header_divergent(
        &self,
        step: &CompiledStep,
        inbound: &Inbound,
        actor: &str,
        leg: &str,
    ) -> bool {
        let Some(method) = inbound.method.as_deref() else { return true };
        if self.background_policy(actor, method, Some(leg)).is_none() {
            return true;
        }
        let (declared, carried) = gate::declared_headers_carried(step, inbound);
        declared == 0 || carried > 0
    }

    /// The step, if any, that holds THIS datagram out for a better candidate.
    ///
    /// Every clause is load-bearing. A background policy must answer the method,
    /// or there is no second reading to hold out for. The step must be one an
    /// armed expect would MATCH, so the hold-out never changes what a datagram
    /// no step wanted does. And the step must have frozen at least one header
    /// while the datagram carries none of them: a step that froze nothing has
    /// nothing to tell the two readings apart with, and one whose headers are
    /// partly there is looking at the message it is about.
    ///
    /// Only ONE datagram is ever held, and only for [`HOLD_OUT_MS`]: the hold
    /// is a tie-break between two arrivals of one instant, never a wait.
    fn holds_out_for_better(
        &self,
        leg: Option<&str>,
        actor: &str,
        inbound: &Inbound,
    ) -> Option<String> {
        if self.held.is_some() {
            return None;
        }
        let leg = leg?;
        let method = inbound.method.as_deref()?;
        self.background_policy(actor, method, Some(leg))?;
        let scope = self.instance.scope();
        let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
        let now = Instant::now();
        self.instance.cursor().frontier().iter().find_map(|id| {
            let step = self.instance.plan().step(id)?;
            let armed = self.armed_at.get(id).is_some_and(|opened| *opened <= now);
            if !armed || !step.is_expect() || step.leg != leg {
                return None;
            }
            if !gate::discriminates(step, inbound).matches()
                || !self.rides_fork(step, inbound).matches()
                || !gate::content_holds(step, inbound, &scope, &resolver).matches()
            {
                return None;
            }
            let (declared, carried) = gate::declared_headers_carried(step, inbound);
            (declared > 0 && carried == 0).then(|| step.id.clone())
        })
    }

    /// Give the held datagram to the background policy it was held against: the
    /// step took a better candidate, so this one was the policy's all along.
    async fn concede_held(&mut self) -> bool {
        let Some(held) = self.held.take() else { return true };
        let Some(method) = held.inbound.method.as_deref() else { return true };
        let Some((index, policy)) =
            self.background_policy(&held.actor, method, Some(&held.leg))
        else {
            return true;
        };
        if let Some(seq) = held.seq {
            self.instance.recording().renote(&held.leg, seq, "background policy");
        }
        self.instance.note_background_answered(index);
        if !self.answer_background(&held.actor, &held.leg, &held.message, policy.status).await {
            self.end_script(Some(&held.leg), None);
            return false;
        }
        true
    }

    /// Nothing better landed inside the hold-out: the datagram goes to the flow
    /// exactly as it would have without the hold.
    async fn release_held(&mut self) -> bool {
        let Some(held) = self.held.take() else { return true };
        self.deliver(
            &held.actor.clone(),
            held.inbound,
            held.message,
            held.raw,
            held.bytes,
            held.repeat,
            held.seq,
        )
        .await
    }

    /// The FLOW half of `dispatch`: attribute the datagram to a leg, learn what
    /// it teaches, and match it against that leg's armed expects.
    ///
    /// `held_seq` names an arrival already in the recording — a datagram
    /// released back into the flow after a hold-out — so a deferral never
    /// doubles a message in the bundle nor restates the instant it landed.
    async fn deliver(
        &mut self,
        actor: &str,
        inbound: Inbound,
        message: SipMessage,
        raw: String,
        bytes: Vec<u8>,
        repeat: bool,
        held_seq: Option<u64>,
    ) -> bool {
        let leg = match self.leg_of(actor, &inbound) {
            Ok(leg) => leg,
            Err(detail) => {
                self.record_arrival("unattributed", raw, None, Some(&detail), repeat);
                // A datagram belonging to no leg of this flow stops nothing: it
                // is the failure it is, and every armed expect is as satisfiable
                // as it was (§11.2).
                self.instance.fail(Failure::UnexpectedDatagram {
                    leg: format!("(actor {actor})"),
                    arrived: inbound.arrived(),
                    detail: Some(detail),
                });
                return true;
            }
        };
        if self.owed_bye_final(&leg, &inbound) {
            self.record_arrival(&leg, raw, None, Some(OWED_BYE_FINAL), repeat);
            return true;
        }
        let seq = match held_seq {
            Some(seq) => {
                self.instance.recording().renote(&leg, seq, HELD_TAKEN);
                Some(seq)
            }
            None => {
                self.record_arrival(&leg, raw, None, None, repeat);
                self.instance.recording().last_seq(&leg)
            }
        };

        // Learn the dialog facts before matching: a step's inline checks may
        // read what this very message taught the leg.
        if let Some(stack) = self.stacks.get_mut(&leg) {
            match &message {
                SipMessage::Request(r) => stack.learn_request(r),
                SipMessage::Response(r) => stack.learn_response(r),
            }
        }
        // Only an INVITE final is ACKed, so only one kind is kept: a 200 to a
        // CANCEL is a final too, and ACKing it would answer the wrong
        // transaction.
        if let SipMessage::Response(r) = &message {
            if r.status() >= 200 && *r.cseq().method() == Method::Invite {
                self.note_final(&leg, r);
            }
        }
        self.sync_leg_state(&leg);

        // The leg's live expects, budget open or not: what a datagram may match
        // is the frontier, and a budget that has not opened yet withholds
        // nothing (§6.8).
        let gated: Vec<CompiledStep> = self
            .instance
            .cursor()
            .frontier()
            .iter()
            .filter_map(|id| self.instance.plan().step(id))
            .filter(|s| s.is_expect() && s.leg == leg)
            .cloned()
            .collect();
        if gated.is_empty() {
            // Nothing on this leg was waiting for anything, so nothing about it
            // became unreachable: the failure stands and the flow goes on
            // (§11.2).
            self.instance.fail(Failure::UnexpectedDatagram {
                leg: leg.clone(),
                arrived: inbound.arrived(),
                detail: None,
            });
            self.answer_unscripted(&leg, &message).await;
            return true;
        }
        let scope = self.instance.scope();
        let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
        // The whole assertion first, over every armed step: a datagram that
        // satisfies it is that step's beyond doubt, so a step whose headers hold
        // always wins one whose headers merely could. Only then the reading that
        // takes a datagram whose HEADER SET diverges (`gate::body_content_holds`)
        // — the step names each difference and fails, rather than refusing the
        // message and abandoning the call several steps past the cause.
        let matched = gated
            .iter()
            .find(|step| {
                gate::discriminates(step, &inbound).matches()
                    && self.rides_fork(step, &inbound).matches()
                    && gate::content_holds(step, &inbound, &scope, &resolver).matches()
            })
            .or_else(|| {
                gated.iter().find(|step| {
                    gate::discriminates(step, &inbound).matches()
                        && self.rides_fork(step, &inbound).matches()
                        && gate::body_content_holds(step, &inbound, &scope).matches()
                        && self.takes_header_divergent(step, &inbound, actor, &leg)
                })
            });
        let Some(step) = matched.cloned() else {
            // Diagnose against the step this datagram came CLOSEST to: one whose
            // discriminator matched and whose content did not says exactly what
            // is wrong, where an unordered group's first member would describe a
            // step the datagram was never for.
            let closest = gated
                .iter()
                .find(|step| gate::discriminates(step, &inbound).matches())
                .unwrap_or(&gated[0]);
            let reason = match gate::discriminates(closest, &inbound) {
                GateVerdict::Rejects(why) => why,
                GateVerdict::Matches => {
                    match self.rides_fork(closest, &inbound) {
                        GateVerdict::Rejects(why) => why,
                        GateVerdict::Matches => {
                            match gate::content_holds(closest, &inbound, &scope, &resolver) {
                                GateVerdict::Rejects(why) => why,
                                GateVerdict::Matches => "no armed expect matched".into(),
                            }
                        }
                    }
                }
            };
            let also_armed: Vec<&str> =
                gated.iter().map(|s| s.id.as_str()).filter(|id| *id != closest.id).collect();
            let reason = if also_armed.is_empty() {
                reason
            } else {
                format!("{reason}; also armed: {}", also_armed.join(", "))
            };
            let step = closest.id.clone();
            self.instance.fail(Failure::UnmatchedDatagram {
                step: step.clone(),
                leg: leg.clone(),
                gated_on: (&closest.discriminator).into(),
                reason,
                arrived: inbound.arrived(),
            });
            self.answer_unscripted(&leg, &message).await;
            // The run goes on unless this arrival left the leg with nothing it
            // can still be satisfied by (§11.2).
            let ladder = self.instance.recording().legs().remove(&leg).unwrap_or_default();
            if !progress::blocks(&inbound, &gated, &ladder) {
                return true;
            }
            self.end_script(Some(&leg), Some(&step));
            return false;
        };

        // A better candidate for the step that was holding one out: the held
        // datagram was the policy's all along, so it goes there now.
        if self.held.as_ref().is_some_and(|h| h.step == step.id) && !self.concede_held().await {
            return false;
        }
        if let Some(seq) = seq {
            self.instance.recording().attribute(&leg, seq, &step.id);
        }
        // The step now owns these bytes: every later copy of them on this leg is
        // a repeat of THIS message, which is the unit §6.9 counts.
        self.repeats.claim(
            &step.id,
            &leg,
            LadderSide::Expect,
            step.retransmits,
            &step.retransmit_intervals_ms,
            &bytes,
            self.now_us(),
        );
        // An OBSERVED fork binds on CONSUMPTION: the step took this arrival, so
        // its id now names the dialog under the To-tag it carried — for every
        // later step naming the id, and for `${early:….tag}` (§6.1).
        if let Some(early) = &step.early {
            if self.early.tag(&leg, early).is_none() && self.learned.tag(&leg, early).is_none() {
                if let Some(tag) = inbound.to_tag.clone() {
                    self.learned.bind(&leg, early, &tag);
                    self.instance.mint_early(early, &leg, &tag);
                }
            }
        }
        self.instance.record_step(&step.id, outcome_of(&inbound));
        // Every header the step declared is READ, and each one that did not hold
        // is recorded by name (§9.1) — the frozen values the match was scoped
        // out of, and the ones it took the datagram in spite of.
        let findings = {
            let resolver =
                Resolver::new(self.instance.state(), &self.instance.config().identities);
            gate::header_findings(&step, &inbound, &resolver)
        };
        for finding in findings {
            self.instance.record(finding);
        }
        // A body assertion a lane-declared known bug took out of the match is
        // still READ, and what it found is recorded — the match was bought, and
        // the verdict says so.
        let waived = gate::waived_body_findings(&step, &inbound, &self.instance.scope());
        for (bug, finding) in waived {
            self.instance.record_waived(bug, finding);
        }
        // A finding is a fact about the verdict, never about the flow: the
        // message matched, so what comes after it is still composable and the
        // run goes on (§11.2) — gating fails the run, not the script.
        self.run_checks(&step, &inbound);
        self.complete(&step);
        true
    }

    /// Whether the FLOW is waiting for this datagram on `leg`: a frontier
    /// expect whose budget has opened and whose discriminator it satisfies.
    ///
    /// A `background` policy answers traffic OUTSIDE the flow (§5.1), and a step
    /// whose window is open is the flow. Both halves are load-bearing on a
    /// policy-answered method the system also RELAYS: the relayed request is
    /// call behaviour a scripted expect owns, while the same method arriving
    /// before that expect's window opens is the replaying SUT's own audit,
    /// whose cadence is a deployment parameter the flow must never gate on.
    fn flow_awaits(&self, leg: &str, inbound: &Inbound) -> bool {
        let now = Instant::now();
        self.instance.cursor().frontier().iter().any(|id| {
            let Some(step) = self.instance.plan().step(id) else { return false };
            step.is_expect()
                && step.leg == leg
                && self.armed_at.get(id).is_some_and(|opened| *opened <= now)
                && gate::discriminates(step, inbound).matches()
        })
    }

    /// Whether `inbound` is the final response a BYE this leg SENT is owed.
    /// RFC 3261 §15.1.2 obliges the UAS to answer a BYE, so the answer is a
    /// protocol fact and not a choreography the document must hold — a capture
    /// whose vantage closed between the BYE and its answer scripts no expect for
    /// it, and the leg that tore the dialog down has nothing left to await.
    /// A document that DOES script the final still owns it, WHATEVER status it
    /// scripts: absorbing a final the document disagrees with would eat the
    /// divergence the confrontation exists to name.
    fn owed_bye_final(&self, leg: &str, inbound: &Inbound) -> bool {
        if !inbound.status.is_some_and(|status| status >= 200)
            || !inbound.cseq_method.eq_ignore_ascii_case("BYE")
            || !self.byes_sent.contains(&(leg.to_string(), inbound.cseq))
        {
            return false;
        }
        !self.instance.plan().steps().iter().any(|step| {
            step.is_expect()
                && step.leg == leg
                && (scripts_a_bye_final(step) || gate::discriminates(step, inbound).matches())
        })
    }

    /// Answer a background policy's message. It is answered and recorded; the
    /// flow never sees it.
    async fn answer_background(
        &mut self,
        actor: &str,
        leg: &str,
        message: &SipMessage,
        status: u16,
    ) -> bool {
        let SipMessage::Request(request) = message else { return true };
        let Some(agent) = self.lane.agents.get(actor).cloned() else { return true };
        let response = sip_message::generators::generate_response(
            request,
            status,
            reason_for(status),
            &sip_message::generators::GenerateResponseOpts {
                to_tag: Some(format!("{leg}-bg")),
                ..Default::default()
            },
        );
        let wire = sip_message::serialize(&SipMessage::Response(response));
        let (dst, note) = match via_target(request) {
            Some(addr) => (addr, "background answer"),
            None => (
                self.lane.route_target,
                "background answer; the request named no reachable Via sent-by, so it is \
                 addressed at the lane's route target",
            ),
        };
        match agent.try_send_datagram(&wire, dst).await {
            Ok(()) => {
                self.instance.recording().push(
                    leg,
                    Dir::Out,
                    self.now_us(),
                    String::from_utf8_lossy(&wire).into_owned(),
                    None,
                    Some(note),
                );
                true
            }
            Err(e) => {
                self.instance.fail(Failure::SendFailed {
                    step: "(background)".into(),
                    leg: leg.to_string(),
                    detail: e.to_string(),
                });
                false
            }
        }
    }

    /// One datagram arriving during the settle window.
    ///
    /// Absorption and `background` behave exactly as they do mid-flow — a
    /// provable repeat is absorbed and noted, a policy's traffic is answered —
    /// and everything else is RECORDED. It is a failure only when the flow had
    /// succeeded: then nothing was left to arrive, and something did.
    async fn record_during_settle(&mut self, actor: &str, message: SipMessage, flow_ok: bool) {
        let inbound = Inbound::of(&message);
        let bytes: Vec<u8> = match &message {
            SipMessage::Request(r) => r.image().to_vec(),
            SipMessage::Response(r) => r.image().to_vec(),
        };
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let leg = self
            .leg_by_call_id(&inbound.call_id)
            .unwrap_or_else(|| "unattributed".to_string());

        let sighting = self.absorption.sight(&bytes, &message);
        let repeat = sighting.repeat;
        if sighting.owner == Owner::TxnDuplicate {
            self.repeats.note(&leg, &bytes, self.now_us());
            self.record_arrival(
                &leg,
                raw,
                None,
                Some("absorbed during settle: byte-identical retransmission"),
                repeat,
            );
            return;
        }
        // A declared ladder's tail legitimately lands in the settle window: it is
        // counted and recorded there exactly as it is mid-flow, and it is not the
        // late arrival that fails a completed flow.
        if let Some(step) = self.repeats.note(&leg, &bytes, self.now_us()) {
            let note = format!("retransmission of the datagram step {step:?} matched, during settle");
            self.instance.recording().push_repeat(
                &leg,
                Dir::In,
                self.now_us(),
                raw,
                None,
                Some(&note),
            );
            self.draw_for_repeat(&message).await;
            return;
        }
        if let Some(method) = &inbound.method {
            if let Some((index, policy)) =
                self.background_policy(actor, method, self.leg_by_call_id(&inbound.call_id).as_deref())
            {
                self.record_arrival(&leg, raw, None, Some("background policy, during settle"), repeat);
                self.instance.note_background_answered(index);
                self.answer_background(actor, &leg, &message, policy.status).await;
                return;
            }
        }
        // A leg whose script ENDED still has a UA behind it, and the generic
        // close answers out of the same dialog state the flow used. So the stack
        // learns what its leg takes here too — a request it never took is one it
        // cannot answer (§11.2). A run still following its flow learns nothing
        // after the flow: nothing composes another message on it.
        if self.abandoned.is_some() {
            if let Some(stack) = self.stacks.get_mut(&leg) {
                match &message {
                    SipMessage::Request(r) => stack.learn_request(r),
                    SipMessage::Response(r) => stack.learn_response(r),
                }
            }
        }
        if self.owed_bye_final(&leg, &inbound) {
            self.record_arrival(&leg, raw, None, Some(OWED_BYE_FINAL), repeat);
            return;
        }
        self.record_arrival(
            &leg,
            raw,
            None,
            Some(if flow_ok {
                "arrived after the flow completed"
            } else if self.abandoned.is_some() {
                "arrived after the script ended, during the generic close"
            } else {
                "arrived during settle, after the flow had already failed"
            }),
            repeat,
        );
        // One diagnosis, not one per datagram: a system tearing down after a
        // completed flow can send several, and they are all the same finding.
        if flow_ok && !self.late_datagram_reported {
            self.late_datagram_reported = true;
            self.instance.fail(Failure::DatagramAfterFlow { leg, arrived: inbound.arrived() });
        }
    }

    /// A run that CANNOT GO ON ends its SCRIPT (§11.2), whatever its polarity.
    ///
    /// The failure is untouched: raised with its site and its evidence, the
    /// datagram recorded verbatim, whatever this does. What it marks is only
    /// that the run stops FOLLOWING the flow — [`close`](crate::close) then ends
    /// what the scripted legs still hold and the run settles, instead of walking
    /// a tail whose next message it can no longer compose. A failure the run CAN
    /// go on past never reaches here: it is recorded and the flow continues
    /// ([`progress`](crate::progress)).
    fn end_script(&mut self, leg: Option<&str>, step: Option<&str>) {
        if self.abandoned.is_some() {
            return;
        }
        self.abandoned = Some(Abandoned {
            leg: leg.map(str::to_string),
            step: step.map(str::to_string),
            pending: Vec::new(),
            closed: Vec::new(),
        });
    }

    /// State the abandonment in the verdict: where the script stopped, what it
    /// never ran, and what the close emitted in its place (§11.2).
    fn state_abandonment(&mut self) {
        let Some(mut abandoned) = self.abandoned.take() else { return };
        abandoned.pending = self.instance.cursor().pending();
        self.instance.note_abandoned(abandoned);
    }

    /// What every scripted leg still holds open (§10, §11.2) — the close's own
    /// half of the settle condition, and the work of its next turn. Empty on a
    /// run whose script was not abandoned, and on one whose dialogs are all
    /// terminal.
    fn close_obligations(&self) -> BTreeMap<String, Owed> {
        if self.abandoned.is_none() {
            return BTreeMap::new();
        }
        close::obligations(&self.instance.recording())
            .into_iter()
            .filter(|(leg, owed)| owed.open() && self.agent_of_leg(leg).is_some())
            .collect()
    }

    /// One turn of the generic close (§11.2): every scripted leg that owes an
    /// emission puts exactly one on the wire.
    ///
    /// One per leg per turn deliberately — the obligation is re-read off the
    /// recording each turn, so the ACK a leg owes and the BYE behind it are two
    /// turns, and whatever the platform sends in between is seen before the
    /// second one goes out.
    async fn close_turn(&mut self, owed: BTreeMap<String, Owed>) {
        for (leg, owed) in owed {
            let Some(act) = owed.act() else { continue };
            if self.close_refused.contains(&leg) {
                continue;
            }
            let Some(agent) = self.agent_of_leg(&leg) else { continue };
            let (wire, dst) = match self.compose_close(&leg, &owed, "(close)") {
                Ok(composed) => composed,
                Err(failure) => {
                    self.close_refused.insert(leg);
                    self.instance.fail(failure);
                    continue;
                }
            };
            if let Err(e) = agent.try_send_datagram(&wire, dst).await {
                self.close_refused.insert(leg.clone());
                self.instance.fail(Failure::SendFailed {
                    step: "(close)".into(),
                    leg: leg.clone(),
                    detail: format!("the generic close could not send: {e}"),
                });
                continue;
            }
            let raw = String::from_utf8_lossy(&wire).into_owned();
            let sent = raw.lines().next().unwrap_or_default().to_string();
            self.instance.recording().push(
                &leg,
                Dir::Out,
                self.now_us(),
                raw,
                None,
                Some(&format!("the generic close: this leg {owed}")),
            );
            if let Some(abandoned) = &mut self.abandoned {
                abandoned.closed.push(CloseAct { leg, owed: act, sent });
            }
        }
    }

    /// Discharge the transaction-layer obligation a REFUSED datagram leaves on
    /// its leg (RFC 3261 §9.2, §17.1.1.3).
    ///
    /// The refusal STANDS and the flow is untouched: like a `background` answer
    /// this moves no cursor and satisfies no `expect`. What is owed is
    /// [`close::unscripted`]'s to say, so a method a document declared is never
    /// answered here, and the emission is the leg's own stack — the same
    /// compliant SIP the generic close puts on the wire.
    async fn answer_unscripted(&mut self, leg: &str, message: &SipMessage) {
        // The CANCEL pair is two acts, and the second is read off the recording
        // once the first is on it — the turn the generic close takes, taken here.
        for _ in 0..2 {
            let ladder = self.instance.recording().legs().remove(leg).unwrap_or_default();
            let Some(owed) = close::unscripted(&ladder, message) else { return };
            let Some(agent) = self.agent_of_leg(leg) else { return };
            let (wire, dst) = match self.compose_close(leg, &owed, UNSCRIPTED) {
                Ok(composed) => composed,
                Err(failure) => {
                    self.instance.fail(failure);
                    return;
                }
            };
            if let Err(e) = agent.try_send_datagram(&wire, dst).await {
                self.instance.fail(Failure::SendFailed {
                    step: UNSCRIPTED.into(),
                    leg: leg.to_string(),
                    detail: format!("the transaction obligation could not be sent: {e}"),
                });
                return;
            }
            self.instance.recording().push(
                leg,
                Dir::Out,
                self.now_us(),
                String::from_utf8_lossy(&wire).into_owned(),
                None,
                Some(&format!("the transaction the flow never scripted: this leg {owed}")),
            );
        }
    }

    /// The message one obligation puts on the wire, and where it goes. `site`
    /// names what a refusal is attributed to, since no step owns the emission.
    ///
    /// Composed by the leg's OWN stack, so what is emitted is ordinary compliant
    /// SIP: the same dialog identity, the same CSeq space and the same Via the
    /// flow's own emissions carry.
    fn compose_close(
        &mut self,
        leg: &str,
        owed: &Owed,
        site: &str,
    ) -> Result<(Vec<u8>, std::net::SocketAddr), Failure> {
        let fail = |detail: String| Failure::SendFailed {
            step: site.to_string(),
            leg: leg.to_string(),
            detail,
        };
        let route_target = self.lane.route_target;
        let stack = self.stacks.get_mut(leg).ok_or_else(|| fail("the leg has no stack".into()))?;
        let (message, dst) = match owed {
            Owed::Answer { cseq_method, status } => {
                let answer = crate::stack::Answer {
                    status: *status,
                    reason: reason_for(*status),
                    cseq_method: Some(cseq_method),
                    early_tag: None,
                };
                let response = stack
                    .respond(&answer, &[], Vec::new(), None)
                    .map_err(|e| fail(e.to_string()))?;
                let dst = stack.response_target(Some(cseq_method)).unwrap_or(route_target);
                (SipMessage::Response(response), dst)
            }
            Owed::Ack(response) => (
                SipMessage::Request(
                    stack
                        .ack_for(response, &[], Vec::new(), None, None)
                        .map_err(|e| fail(e.to_string()))?,
                ),
                stack.next_hop(),
            ),
            Owed::Bye => (
                SipMessage::Request(
                    stack
                        .in_dialog(Method::Bye, &[], Vec::new(), None, None)
                        .map_err(|e| fail(e.to_string()))?,
                ),
                stack.next_hop(),
            ),
            Owed::Cancel => (
                SipMessage::Request(stack.cancel(&[]).map_err(|e| fail(e.to_string()))?),
                stack.next_hop(),
            ),
            // Nothing to compose: the caller filtered these out by asking for the
            // act first, and a match without them would be a wildcard.
            Owed::Nothing | Owed::AwaitFinal | Owed::AwaitTeardown => {
                return Err(fail(format!("{owed} puts no message on the wire")))
            }
        };
        Ok((sip_message::serialize(&message), dst))
    }

    /// The background policy that answers `method` at the endpoint `actor` sits
    /// on. A loopback endpoint hosts several actors on one socket, so the policy
    /// belongs to the ENDPOINT's actors, not to whichever one happened to pump.
    fn background_policy(
        &self,
        actor: &str,
        method: &str,
        leg: Option<&str>,
    ) -> Option<(usize, crate::background::Policy)> {
        // The LEG's own actor first, where the datagram rides one: a loopback
        // endpoint hosts two actors on one socket, and crediting both their
        // policies to whichever is declared first makes one counter count twice
        // and the other never.
        let owner = leg.and_then(|leg| self.instance.plan().actor_of_leg(leg));
        if let Some((index, policy)) =
            owner.and_then(|owner| self.instance.background().policy_for(owner, method))
        {
            return Some((index, policy.clone()));
        }
        // Otherwise any actor on the receiving endpoint: out-of-dialog traffic
        // belongs to the socket, not to a dialog.
        let endpoint = self.instance.plan().actor(actor).map(|a| a.endpoint.as_str())?;
        self.instance
            .plan()
            .actors()
            .values()
            .filter(|candidate| candidate.endpoint == endpoint)
            .find_map(|candidate| self.instance.background().policy_for(&candidate.id, method))
            .map(|(index, policy)| (index, policy.clone()))
    }

    /// The leg carrying `call_id`, where one already does.
    fn leg_by_call_id(&self, call_id: &str) -> Option<String> {
        self.stacks
            .iter()
            .find(|(_, stack)| stack.call_id() == call_id)
            .map(|(leg, _)| leg.clone())
    }

    /// Which leg an arriving datagram rides, or why none does.
    ///
    /// A claim is scoped to the SOCKET the INVITE arrived on: `actor` is the one
    /// whose agent surfaced it, and only the legs on that actor's endpoint may
    /// claim it. Offering it to every endpoint would let an `arrival-order`
    /// candidate on another socket steal a dialog it never received.
    fn leg_of(&mut self, actor: &str, inbound: &Inbound) -> Result<String, String> {
        if let Some(leg) = self.leg_by_call_id(&inbound.call_id) {
            return Ok(leg);
        }
        if inbound.method.as_deref().map(Method::from_wire) != Some(Method::Invite) {
            return Err(format!(
                "no leg carries Call-ID {:?}, and only an initial INVITE may open one",
                inbound.call_id
            ));
        }
        let endpoint = self
            .instance
            .plan()
            .actor(actor)
            .map(|a| a.endpoint.clone())
            .ok_or_else(|| format!("actor {actor:?} is not declared, so it hosts no claim"))?;
        let ruri_user = inbound.ruri_user.clone().unwrap_or_default();
        self.instance.claim(&endpoint, &ruri_user, inbound).map_err(|e| e.to_string())
    }

    /// Publish the stack's dialog facts into the state accessors read — the
    /// leg's own, and each fork's where the leg is ringing several (§8.1).
    fn sync_leg_state(&mut self, leg: &str) {
        let Some(stack) = self.stacks.get(leg) else { return };
        let forks: Vec<(String, u32)> = self
            .early
            .iter()
            .filter(|(owner, _, _)| *owner == leg)
            .filter_map(|(_, id, tag)| stack.early_rseq(tag).map(|rseq| (id.to_string(), rseq)))
            .collect();
        let call_id = stack.call_id().to_string();
        let local_tag = stack.local_tag().to_string();
        let remote_tag = stack.remote_tag().map(str::to_string);
        let remote_target = stack.remote_target().to_string();
        let route_set = stack.route_set().to_vec();
        let cseq_local = stack.local_cseq();
        let cseq_remote = stack.remote_cseq();
        let rseq = stack.rseq();
        let state = self.instance.leg_mut(leg);
        state.call_id = Some(call_id);
        state.local_tag = Some(local_tag);
        state.remote_tag = remote_tag;
        state.remote_target = Some(remote_target);
        state.route_set = route_set;
        state.cseq_local = cseq_local;
        state.cseq_remote = cseq_remote;
        state.rseq = rseq;
        for (id, rseq) in forks {
            self.instance.record_early_rseq(&id, rseq);
        }
    }

    /// A matched step's inline checks (§9), each routed by its class (§9.1).
    ///
    /// Every finding lands on the verdict — gating fails the run, a class the
    /// lane does not gate on is informative — and the step completes either
    /// way: a check reads the message, it never makes the flow unfollowable.
    fn run_checks(&mut self, step: &CompiledStep, inbound: &Inbound) {
        if step.checks.is_empty() {
            return;
        }
        let site = format!("step {:?}", step.id);
        let findings: Vec<Finding> = {
            let resolver =
                Resolver::new(self.instance.state(), &self.instance.config().identities);
            step.checks
                .iter()
                .filter_map(|check| {
                    checks::evaluate(&site, check, &MessageObservables(inbound), &resolver)
                        .map(|failure| Finding::new(check.class, failure))
                })
                .collect()
        };
        for finding in findings {
            self.instance.record(finding);
        }
    }

    /// Mark a step done and remember when, for the dwells anchored on it.
    fn complete(&mut self, step: &CompiledStep) {
        let now = Instant::now();
        self.measure_timer(step, now);
        self.completed_at.insert(step.id.clone(), now);
        for released in self.instance.complete_step(&step.id) {
            self.completed_at.insert(released, now);
        }
        self.settle_blocks(now);
    }

    /// Stamp every block that has just completed, for the dwells anchored on it.
    ///
    /// `alt` scoping leaves the block's own id as the only legal way to anchor
    /// on a branch that ran, so a dwell measured from one must have a time to
    /// measure from. Every path that can retire a step calls this: a block whose
    /// last member is a RELEASED tolerated absence completes exactly as one
    /// whose last member arrived.
    fn settle_blocks(&mut self, now: Instant) {
        let settled: Vec<String> = self
            .instance
            .plan()
            .program()
            .items
            .iter()
            .filter(|item| !self.completed_at.contains_key(&item.id))
            .filter(|item| self.instance.cursor().node_complete(&item.id))
            .map(|item| item.id.clone())
            .collect();
        for id in settled {
            self.completed_at.insert(id, now);
        }
    }

    /// A timer-anchored arrival, measured against what the document declares
    /// (§9.2).
    ///
    /// It reads EXPECT steps alone: a `timer_linked` dwell on an expect is what
    /// the SYSTEM's own timer measured, while the same dwell on a send is one
    /// this interpreter itself slept and has nothing to observe. The reading is
    /// recorded whatever it says, and it FAILS the run when the run's stated
    /// tolerance does not cover the difference — a window absorbs a delta, never
    /// an absence and never an order.
    fn measure_timer(&mut self, step: &CompiledStep, now: Instant) {
        if !step.is_expect() || !step.delay.timer_linked {
            return;
        }
        let Some(anchor) = self.anchor_at(&step.delay.from) else { return };
        let declared_ms = step.delay.ms;
        let observed_ms = now.saturating_duration_since(anchor).as_millis() as u64;
        let tolerance_ms = self.instance.config().timing_tolerance_ms;
        let absorbed = self.instance.config().absorbs_timing(declared_ms, observed_ms);
        self.instance.note_timing(TimingNote {
            step: step.id.clone(),
            leg: step.leg.clone(),
            declared_ms,
            observed_ms,
            delta_ms: observed_ms as i64 - declared_ms as i64,
            tolerance_ms,
        });
        if !absorbed {
            self.instance.record(Finding::gating(Failure::TimingOutOfTolerance {
                step: step.id.clone(),
                leg: step.leg.clone(),
                declared_ms,
                observed_ms,
                tolerance_ms,
            }));
        }
    }

    /// Emit one send step.
    async fn emit(&mut self, step: &CompiledStep) -> bool {
        let effects = StepEffects::of(self.instance.plan().deviations_for(&step.id));
        let refusals = effects.refusals();
        if !refusals.is_empty() {
            for failure in refusals {
                self.instance.fail(failure);
            }
            return false;
        }
        // A withheld automatic is not sent, and the flow moves past it: the
        // deviation IS the absence.
        if effects.suppressed {
            self.instance.recording().push(
                &step.leg,
                Dir::Out,
                self.now_us(),
                String::new(),
                Some(&step.id),
                Some("withheld by a suppress-auto deviation"),
            );
            self.complete(step);
            return true;
        }
        let message = match self.compose(step, &effects) {
            Ok(message) => message,
            Err(failure) => {
                self.instance.fail(failure);
                return false;
            }
        };
        let wire = sip_message::serialize(&message);
        // An auto ACK's count is DRAWN by the wire, not paced (§6.3): one copy
        // per repeat of the final its transaction drew.
        let draws = drawn_key(step, &message);
        // A ladder this peer cannot pace refuses BEFORE the wire: emitting the
        // first copy and then discovering the repeats have no interval would
        // leave a half-reproduced ladder on the wire (§14).
        let ladder = match step.retransmits.filter(|n| *n > 0) {
            Some(_) if draws.is_some() => None,
            None => None,
            Some(count) => match retransmit::schedule_of(&wire, &step.retransmit_intervals_ms) {
                Ok(schedule) => Some((schedule, count)),
                Err(detail) => {
                    self.instance.fail(Failure::RetransmitLadderRefused {
                        step: step.id.clone(),
                        leg: step.leg.clone(),
                        detail,
                    });
                    return false;
                }
            },
        };
        let Some(actor) = self.instance.plan().actor_of_leg(&step.leg).map(str::to_string) else {
            self.instance.fail(Failure::SendFailed {
                step: step.id.clone(),
                leg: step.leg.clone(),
                detail: "the leg names no actor".into(),
            });
            return false;
        };
        let Some(agent) = self.lane.agents.get(&actor).cloned() else {
            self.instance.fail(Failure::SendFailed {
                step: step.id.clone(),
                leg: step.leg.clone(),
                detail: format!("the lane bound no agent for actor {actor:?}"),
            });
            return false;
        };
        // A response goes back where the request came from (§18.2.2); a request
        // goes to the lane's route target.
        let mut fallback: Option<String> = None;
        let dst = match self.stacks.get(&step.leg) {
            None => self.lane.route_target,
            Some(stack) if step.msg.status.is_some() => {
                match stack.response_target(step.msg.cseq_method.as_deref()) {
                    Some(addr) => addr,
                    None => {
                        fallback = Some(
                            "the answered request named no reachable Via sent-by; \
                             addressed at the lane's route target"
                                .to_string(),
                        );
                        stack.next_hop()
                    }
                }
            }
            Some(stack) => stack.next_hop(),
        };
        if let Err(e) = agent.try_send_datagram(&wire, dst).await {
            self.instance.fail(Failure::SendFailed {
                step: step.id.clone(),
                leg: step.leg.clone(),
                detail: e.to_string(),
            });
            return false;
        }
        let raw = String::from_utf8_lossy(&wire).into_owned();
        self.instance.recording().push(
            &step.leg,
            Dir::Out,
            self.now_us(),
            raw,
            Some(&step.id),
            fallback.as_deref(),
        );
        // §15.1.2 owes this BYE a final response, and it arrives whether or not
        // the capture's vantage lasted long enough for the document to script
        // an expect for it.
        if let SipMessage::Request(request) = &message {
            if *request.method() == Method::Bye {
                self.byes_sent.insert((step.leg.clone(), request.cseq().seq()));
            }
        }
        // An expect-side ladder on this leg ends here where this is its answer:
        // the SUT stops on it, so a rung emitted just before it was a race.
        self.repeats.answered(&step.leg, &wire, self.now_us());
        self.repeats.claim(
            &step.id,
            &step.leg,
            LadderSide::Send,
            step.retransmits,
            &step.retransmit_intervals_ms,
            &wire,
            self.now_us(),
        );
        if let Some((schedule, count)) = ladder {
            self.schedule_ladder(step, &agent, &wire, dst, schedule, count);
        }
        if let Some(key) = draws {
            let ack = DrawnAck {
                step: step.id.clone(),
                leg: step.leg.clone(),
                wire: wire.clone(),
                dst,
            };
            // Every copy of the final that arrived while this ACK was HELD is
            // still owed one (RFC 3261 §13.2.2.4), so the hold releases them all.
            let owed = self.drawn.sent(key, ack.clone());
            for _ in 0..owed {
                if !self.draw_ack(&ack).await {
                    return false;
                }
            }
        }
        self.instance.record_step(&step.id, outcome_of(&Inbound::of(&message)));
        self.sync_leg_state(&step.leg);
        self.complete(step);
        true
    }

    /// The agent that plays `leg`, if the lane bound one.
    fn agent_of_leg(&self, leg: &str) -> Option<Agent> {
        let actor = self.instance.plan().actor_of_leg(leg)?;
        self.lane.agents.get(actor).cloned()
    }

    /// Emit one more copy of an ACK that a repeat of its final drew (§6.3): the
    /// SAME datagram on the SAME client transaction, since RFC 3261 §13.2.2.4
    /// owes one ACK per 2xx received and every one of them answers the INVITE
    /// the first 2xx armed.
    async fn draw_ack(&mut self, ack: &DrawnAck) -> bool {
        let Some(agent) = self.agent_of_leg(&ack.leg) else {
            self.instance.fail(Failure::SendFailed {
                step: ack.step.clone(),
                leg: ack.leg.clone(),
                detail: "the leg names no bound agent to draw its ACK from".into(),
            });
            return false;
        };
        if let Err(e) = agent.try_send_datagram(&ack.wire, ack.dst).await {
            self.instance.fail(Failure::SendFailed {
                step: ack.step.clone(),
                leg: ack.leg.clone(),
                detail: format!("drawing an ACK for a repeated final: {e}"),
            });
            return false;
        }
        self.instance.recording().push_repeat(
            &ack.leg,
            Dir::Out,
            self.now_us(),
            String::from_utf8_lossy(&ack.wire).into_owned(),
            Some(&ack.step),
            Some("drawn by a repeat of the final it acknowledges"),
        );
        self.repeats.answered(&ack.leg, &ack.wire, self.now_us());
        self.repeats.note(&ack.leg, &ack.wire, self.now_us());
        true
    }

    /// A repeat of a 2xx draws the ACK an auto step already sent for it, where
    /// the document declared that ladder. Every other repeat draws nothing.
    async fn draw_for_repeat(&mut self, message: &SipMessage) -> bool {
        let SipMessage::Response(response) = message else { return true };
        let Some(key) = ack_key_of_final(response) else { return true };
        let Some(ack) = self.drawn.repeat_sighted(key) else { return true };
        self.draw_ack(&ack).await
    }

    /// Book a send step's declared retransmission ladder (§6.9): `count` more
    /// copies of the same bytes, each at the instant `schedule` paces it, counting
    /// from the copy just sent. The loop emits them; nothing here waits.
    ///
    /// The peer stays RFC-compliant while it does it — the copies are identical,
    /// and the intervals are the RFC's own — and each one is recorded pointing
    /// at the datagram it repeats. The flow DOES advance meanwhile: a ladder
    /// paces one leg's transaction and gates nothing else, so sleeping the rungs
    /// out here would park every other leg's dwell and every arrival's turn
    /// behind a timer that was never a barrier.
    fn schedule_ladder(
        &mut self,
        step: &CompiledStep,
        agent: &Agent,
        wire: &[u8],
        dst: std::net::SocketAddr,
        schedule: Schedule,
        count: u32,
    ) {
        let mut at = Instant::now();
        for n in 1..=count {
            // `schedule_of` refuses a class with no rung, so every rung a
            // declared count names has its wait.
            let Some(wait) = schedule.interval(n) else { break };
            at += wait;
            self.pending_repeats.push(PendingRepeat {
                step: step.id.clone(),
                leg: step.leg.clone(),
                agent: agent.clone(),
                wire: wire.to_vec(),
                dst,
                at,
                n,
                count,
            });
        }
    }

    /// The soonest rung still owed, for the loop's own wake calculus.
    fn next_repeat(&self) -> Option<Instant> {
        self.pending_repeats.iter().map(|r| r.at).min()
    }

    /// Emit every rung now due, oldest first. `false` once a send fails.
    async fn drain_repeats(&mut self) -> bool {
        loop {
            let now = Instant::now();
            let Some(i) = self
                .pending_repeats
                .iter()
                .enumerate()
                .filter(|(_, r)| r.at <= now)
                .min_by_key(|(_, r)| (r.at, r.n))
                .map(|(i, _)| i)
            else {
                return true;
            };
            let due = self.pending_repeats.remove(i);
            if let Err(e) = due.agent.try_send_datagram(&due.wire, due.dst).await {
                self.instance.fail(Failure::SendFailed {
                    step: due.step.clone(),
                    leg: due.leg.clone(),
                    detail: format!("retransmission {} of {}: {e}", due.n, due.count),
                });
                return false;
            }
            self.instance.recording().push_repeat(
                &due.leg,
                Dir::Out,
                self.now_us(),
                String::from_utf8_lossy(&due.wire).into_owned(),
                Some(&due.step),
                Some(&format!("retransmission {} of {}", due.n, due.count)),
            );
            self.repeats.note(&due.leg, &due.wire, self.now_us());
        }
    }

    /// Record one ARRIVING datagram, with the §17.2 seam deciding whether it is
    /// a repeat: a repeat carries its `repeat_of` back-reference to the datagram
    /// it repeats (friction H8), everything else does not. The classification is
    /// the seam's alone — this only chooses which recording door to use.
    fn record_arrival(
        &self,
        leg: &str,
        raw: String,
        step: Option<&str>,
        note: Option<&str>,
        repeat: bool,
    ) {
        let recording = self.instance.recording();
        let at_us = self.now_us();
        if repeat {
            recording.push_repeat(leg, Dir::In, at_us, raw, step, note);
        } else {
            recording.push(leg, Dir::In, at_us, raw, step, note);
        }
    }

    /// The ladders the run counted (§6.9), into the verdict — and a ladder that
    /// is not the one its emitter owed is a failure, on every lane: a
    /// retransmission count is a protocol fact, never a lane's vocabulary.
    fn reconcile_retransmits(&mut self) {
        // The run's own end bounds a ladder nothing closed (§6.9): it ran until
        // the run stopped, and that is the window its rungs are owed inside.
        let run_end_us = self.now_us();
        let notes = self.repeats.notes(run_end_us);
        for failure in self.repeats.mismatches(run_end_us) {
            self.instance.record(Finding::gating(failure));
        }
        self.instance.note_retransmits(notes);
    }

    /// Keep one INVITE final per transaction on its leg, in arrival order. A
    /// second final of the same transaction REPLACES the first (a 2xx after a
    /// provisional-then-final race answers the same INVITE); a fork's own 2xx,
    /// which carries another To-tag, is its own entry.
    fn note_final(&mut self, leg: &str, response: &SipResponse) {
        let key = |r: &SipResponse| {
            (r.cseq().seq(), r.to().tag().map(str::to_string).unwrap_or_default())
        };
        let pool = self.finals.entry(leg.to_string()).or_default();
        match pool.iter_mut().find(|held| key(held) == key(response)) {
            Some(held) => *held = response.clone(),
            None => pool.push(response.clone()),
        }
    }

    /// The final an auto ACK step acknowledges: the newest one still answering an
    /// un-ACKed INVITE this leg sent, else the newest answering any of them.
    ///
    /// LEG STATE names the transaction, never the step's captured `cseq` (§6.3)
    /// — the same resolution `Stack::ack_for` composes against, fallback
    /// included. RFC 3261 §14.1 leaves one INVITE outstanding per dialog, so a
    /// compliant peer offers one candidate. A captured peer that pipelined a
    /// second re-INVITE before ACKing the first's 2xx offers two, and each ACK
    /// DISCHARGES its own, so the deferred 2xx is still standing for the step
    /// that owes it rather than losing its ACK to the newer transaction. A
    /// captured peer that ACKed one 2xx TWICE (§13.2.2.4 obliges one ACK per 2xx
    /// received) leaves none outstanding, and the discharge must not deny the
    /// second step the final it is modelling an ACK of.
    fn final_for(&self, step: &CompiledStep) -> Option<SipResponse> {
        let stack = self.stacks.get(&step.leg)?;
        let outstanding: BTreeSet<u32> = stack.outstanding_invites().collect();
        let sent: BTreeSet<u32> = stack.sent_invite_cseqs().collect();
        let pool = self.finals.get(&step.leg)?;
        let newest = |seqs: &BTreeSet<u32>| pool.iter().rev().find(|r| seqs.contains(&r.cseq().seq()));
        newest(&outstanding).or_else(|| newest(&sent)).cloned()
    }

    /// The CSeqs of every INVITE `leg` has sent, in send order: the transactions
    /// [`Self::final_for`] looks through, so a refusal names what it looked at.
    fn sent_invites(&self, leg: &str) -> Vec<u32> {
        self.stacks.get(leg).map(|s| s.sent_invite_cseqs().collect()).unwrap_or_default()
    }

    /// Compose the message a send step emits.
    fn compose(
        &mut self,
        step: &CompiledStep,
        effects: &StepEffects,
    ) -> Result<SipMessage, Failure> {
        let site = format!("step {:?}", step.id);
        // The lane's per-call directives (§4.3) reach exactly one step: the
        // INVITE that opens this call's caller leg. Every other send carries the
        // run-wide headers alone.
        let call_headers = self
            .instance
            .plan()
            .call_dialled_by(&step.id)
            .and_then(|call| self.instance.config().headers_for_call(call))
            .cloned()
            .unwrap_or_default();
        let (rendered, cseq_override) = {
            let resolver =
                Resolver::new(self.instance.state(), &self.instance.config().identities);
            let cx = render::Context {
                resolver: &resolver,
                config: self.instance.config(),
                effects,
                composer: self.lane.composer,
                base_dir: &self.lane.base_dir,
                media: &self.lane.media,
                leg: &step.leg,
                call_headers: &call_headers,
            };
            let rendered =
                render::render(&step.msg, &site, &cx).map_err(|e| Failure::SendFailed {
                    step: step.id.clone(),
                    leg: step.leg.clone(),
                    detail: e.to_string(),
                })?;
            // A relative override reads a CSeq the run already produced, so it
            // resolves HERE, against run state, and never against document text.
            let cseq_override = match &effects.cseq_override {
                None => None,
                Some((deviation, value)) => Some((
                    deviation.clone(),
                    match value {
                        CseqValue::Absolute(n) => *n,
                        CseqValue::Relative(computed) => {
                            resolver.computed(computed).map_err(|e| Failure::SendFailed {
                                step: step.id.clone(),
                                leg: step.leg.clone(),
                                detail: format!("cseq-override {deviation:?}: {e}"),
                            })?
                        }
                    },
                )),
            };
            (rendered, cseq_override)
        };
        let body = rendered.template.body().to_vec();
        // A preserved emission (§11) keeps a stated Content-Type IN the stored
        // block: lifting it out would frame the body under a header the stack
        // places, which is exactly the reordering the deviation forbids.
        let preserved = !effects.preserve_stored_block.is_empty();
        let content_type = rendered
            .template
            .headers()
            .iter()
            .find(|h| sip_message::HeaderName::ContentType.matches(&h.name))
            .filter(|_| !preserved)
            .map(|h| h.value.clone());
        let headers: Vec<sip_message::TemplateHeader> = rendered
            .template
            .headers()
            .iter()
            .filter(|h| preserved || !sip_message::HeaderName::ContentType.matches(&h.name))
            .cloned()
            .collect();
        let fail = |detail: String| Failure::SendFailed {
            step: step.id.clone(),
            leg: step.leg.clone(),
            detail,
        };
        let acked_final = self.final_for(step);
        let sent_invites = self.sent_invites(&step.leg);
        let early_tag = self.fork_tag_of(step).map(str::to_string);
        let stack = self
            .stacks
            .get_mut(&step.leg)
            .ok_or_else(|| fail("the leg has no stack".into()))?;

        // Where the CSeq is NOT the stack's to choose, an override cannot be
        // honoured, and a message that quietly kept the compliant number would
        // not reproduce the defect (§11).
        if let Some((deviation, _)) = &cseq_override {
            let owned = crate::deviation::cseq_override_refusal(
                step.msg.status,
                step.msg.method.as_deref(),
            );
            if let Some(reason) = owned {
                return Err(Failure::CseqOverrideRefused {
                    step: step.id.clone(),
                    leg: step.leg.clone(),
                    deviation: deviation.clone(),
                    reason: reason.to_string(),
                });
            }
        }
        let cseq_override = cseq_override.map(|(_, value)| value);

        if let Some(status) = step.msg.status {
            let answer = crate::stack::Answer {
                status,
                reason: step.msg.reason.as_deref().unwrap_or_else(|| reason_for(status)),
                cseq_method: step.msg.cseq_method.as_deref(),
                early_tag: early_tag.as_deref(),
            };
            let response = stack
                .respond(&answer, &headers, body, content_type)
                .map_err(|e| fail(e.to_string()))?;
            let message = SipMessage::Response(response);
            preserved_block(step, effects, &rendered, &message)?;
            return Ok(message);
        }
        let method = Method::from_wire(step.msg.method.as_deref().unwrap_or_default());
        let request = match method {
            // The stack owns the ACK's COMPOSITION — its CSeq is the INVITE's,
            // never the step's stored one, and only a `cseq-override` (§11)
            // states another — and the step owns its content: the frozen
            // headers, and on a 2xx the body a delayed offer's answer rides
            // (RFC 3261 §13.2.1).
            Method::Ack => {
                let response = acked_final.ok_or_else(|| {
                    fail(if sent_invites.is_empty() {
                        "no final response to ACK: this leg sent no INVITE".into()
                    } else {
                        let cseqs: Vec<String> =
                            sent_invites.iter().map(u32::to_string).collect();
                        format!(
                            "no final response to ACK: nothing has answered any INVITE this \
                             leg sent, CSeq {}",
                            cseqs.join(", ")
                        )
                    })
                })?;
                stack
                    .ack_for(&response, &headers, body, content_type, cseq_override)
                    .map_err(|e| fail(e.to_string()))?
            }
            Method::Cancel => stack.cancel(&headers).map_err(|e| fail(e.to_string()))?,
            // The stack owns the RAck (RFC 3262 §7.2): it acknowledges a
            // provisional the leg RECEIVED, and a PRACK rides an early dialog
            // that is not confirmed, so it does not take the in-dialog path.
            // The fork the step names picks WHICH early dialog, as on UPDATE.
            Method::Prack => stack
                .prack(early_tag.as_deref(), &headers, body, content_type, cseq_override)
                .map_err(|e| fail(e.to_string()))?,
            // RFC 3311 §5.1: an UPDATE runs inside a dialog that may still be
            // early, so the stack decides between the confirmed dialog and the
            // fork the step names.
            Method::Update => stack
                .update(early_tag.as_deref(), &headers, body, content_type, cseq_override)
                .map_err(|e| fail(e.to_string()))?,
            _ if !stack.has_dialog() && stack.sent_invite().is_none() => {
                // A dialog-opening request is addressed at a party. Emitting one
                // with an empty Request-URI, From or To puts a malformed message
                // on the wire and calls it a replay.
                let missing: Vec<&str> = [
                    ("ruri", rendered.ruri.as_deref()),
                    ("from", rendered.from.as_deref()),
                    ("to", rendered.to.as_deref()),
                ]
                .iter()
                .filter(|(_, value)| value.is_none_or(str::is_empty))
                .map(|(what, _)| *what)
                .collect();
                if !missing.is_empty() {
                    return Err(fail(format!(
                        "the dialog-opening {method} states no {}; the document's tier-2 refs \
                         resolve to nothing the lane can address",
                        missing.join(", ")
                    )));
                }
                let addresses = crate::stack::Addresses {
                    ruri: rendered.ruri.as_deref().unwrap_or_default(),
                    from: rendered.from.as_deref().unwrap_or_default(),
                    to: rendered.to.as_deref().unwrap_or_default(),
                };
                stack
                    .out_of_dialog(method, &addresses, &headers, body, content_type, cseq_override)
                    .map_err(|e| fail(e.to_string()))?
            }
            _ => stack
                .in_dialog(method, &headers, body, content_type, cseq_override)
                .map_err(|e| fail(e.to_string()))?,
        };
        let message = SipMessage::Request(request);
        preserved_block(step, effects, &rendered, &message)?;
        Ok(message)
    }

    /// The settle phase (§10). Failing to settle is always run failure.
    ///
    /// The sockets KEEP BEING PUMPED here, and everything that arrives is
    /// recorded: the settle window is exactly where a teardown the system owes
    /// shows up, and a run that stopped listening cannot corroborate its own
    /// "one active call" from the wire. What a datagram MEANS in this window
    /// depends on `flow_ok`: after a flow that succeeded it is a late arrival
    /// nothing scripted and it FAILS the run; after a flow that already failed
    /// it is the aftermath of that failure and is recorded with a note, since
    /// stacking a second diagnosis on the first says nothing new.
    async fn settle(&mut self, sut: &dyn Sut, budget_ms: u64, flow_ok: bool) -> Option<u64> {
        let post = self.instance.plan().document().postconditions.clone();
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        let mut timed_out = false;
        let mut stall = Stall::new(self.wall_ceiling());
        loop {
            if let Some(failure) = stall.check("settle", || {
                let done = self.instance.cursor().is_done() || self.abandoned.is_some();
                let mut open = settle::open_reasons(done, sut, post.as_ref());
                open.extend(close_reasons(&self.close_obligations()));
                open
            }) {
                self.instance.fail(failure);
                return None;
            }
            // A script a declared divergence ENDED is a flow that is over by
            // ruling (§11.2), and what is still open on it is the close's.
            let flow_done = self.instance.cursor().is_done() || self.abandoned.is_some();
            let owed = self.close_obligations();
            // A rung still owed keeps the run open: the flow ending does not
            // end a transaction's own timer, and `drive` counts the ladders
            // only after settle for exactly that reason.
            if owed.is_empty()
                && self.pending_repeats.is_empty()
                && settle::is_settled(flow_done, sut, post.as_ref())
            {
                break;
            }
            if Instant::now() >= deadline {
                let mut open = settle::open_reasons(flow_done, sut, post.as_ref());
                open.extend(close_reasons(&owed));
                self.instance.fail(Failure::SettleTimedOut { budget_ms, open });
                timed_out = true;
                break;
            }
            // A ladder outliving the flow keeps its own clock here: `drive`
            // counts the rungs only after settle for exactly this reason.
            if !self.drain_repeats().await {
                break;
            }
            // Whatever the close still owes goes out BEFORE the loop waits again,
            // so the answer to it is what the next turn drains.
            self.close_turn(owed).await;
            // Poll in small steps so the settle condition is re-read as the
            // system tears down, and drain whatever arrives in between.
            let mut wake = deadline.min(Instant::now() + Duration::from_millis(20));
            if let Some(rung) = self.next_repeat() {
                wake = wake.min(rung);
            }
            match self.wait(wake).await {
                Wake::Timer => {}
                Wake::Message { actor, message } => {
                    self.record_during_settle(&actor, *message, flow_ok).await;
                }
                Wake::Closed { actor, detail } => {
                    self.instance.fail(Failure::TransportClosed { actor, detail });
                    break;
                }
            }
        }
        // A `background` count is a fact about the PERIOD the run covered (§5.1),
        // and the period is over whether or not the system settled. So the
        // counters are read either way: a run that ran out of budget still says
        // what its endpoints heard, and an absence counter still catches the
        // traffic that should never have reached them.
        for failure in self.instance.background().settle_failures() {
            self.instance.fail(failure);
        }
        if timed_out {
            return None;
        }
        let settled_at = self.now_ms();
        let findings = {
            let resolver =
                Resolver::new(self.instance.state(), &self.instance.config().identities);
            settle::evaluate(sut, post.as_ref(), &resolver)
        };
        for finding in findings {
            self.instance.record(finding);
        }
        Some(settled_at)
    }
}

/// What the generic close still holds open, in its own words: the settle's own
/// vocabulary for a scripted dialog that is not terminal yet (§10).
fn close_reasons(owed: &BTreeMap<String, Owed>) -> Vec<String> {
    owed.iter().map(|(leg, owed)| format!("the generic close: leg {leg} {owed}")).collect()
}

/// A stall detector: how many consecutive turns a loop has taken without the
/// clock moving, and how much wall time it has burned.
///
/// A turn that neither advances virtual time nor changes the run's position is
/// a turn that will repeat forever. The bounds are generous — a busy settle
/// window handles many datagrams inside one virtual instant, and a real clock
/// spends its document's whole declared timeline — and their only job is to turn
/// a hang into a diagnosis.
struct Stall {
    last: Instant,
    started: std::time::Instant,
    /// Wall time this run may burn, derived from its clock and the timeline its
    /// document declares.
    wall: Duration,
    turns: u32,
    total: u64,
}

impl Stall {
    /// Turns at ONE instant before a loop is declared stuck: neither the clock
    /// nor the run is moving.
    const LIMIT: u32 = 5_000;
    /// Turns in TOTAL before a loop is declared stuck: the clock is moving, in
    /// steps too small to ever reach a deadline. Generous — a long call polls
    /// its settle window thousands of times.
    const TOTAL: u64 = 2_000_000;

    fn new(wall: Duration) -> Self {
        Stall { last: Instant::now(), started: std::time::Instant::now(), wall, turns: 0, total: 0 }
    }

    /// `Some` once the loop cannot be making progress, by any of the three
    /// measures. The failure names the phase and what the run was waiting for.
    fn check(
        &mut self,
        phase: &str,
        waiting_on: impl FnOnce() -> Vec<String>,
    ) -> Option<Failure> {
        let now = Instant::now();
        self.total += 1;
        if now != self.last {
            self.last = now;
            self.turns = 0;
        } else {
            self.turns += 1;
        }
        let stuck = self.turns > Self::LIMIT
            || self.total > Self::TOTAL
            || self.started.elapsed() > self.wall;
        if !stuck {
            return None;
        }
        let mut detail = waiting_on();
        detail.push(format!(
            "after {} turn(s), {} at one instant, {:?} of wall time",
            self.total,
            self.turns,
            self.started.elapsed()
        ));
        Some(Failure::RunStalled { phase: phase.to_string(), waiting_on: detail })
    }
}

/// What woke the run loop.
enum Wake {
    /// A deadline, or a per-receive timeout that means "nothing yet".
    Timer,
    /// A datagram at one actor. Boxed: a parsed message dwarfs the other
    /// variants, and this enum is created once per loop turn.
    Message { actor: String, message: Box<SipMessage> },
    /// A socket closed or errored under the run, in the transport's own words.
    Closed { actor: String, detail: String },
}

/// The dialog an auto ACK step's DRAWN ladder is keyed on (§6.3), or `None`
/// wherever a count is not drawn.
///
/// Three conditions, and each is the ruling's: only an ACK's count is drawn
/// (every other message has an RFC ladder to ride), only an `auto` step's is
/// (the stack owns the ACK), and only one the document MARKS with `cseq` (§6.3
/// — a count is measured against the repeats of one transaction's final).
fn drawn_key(step: &CompiledStep, message: &SipMessage) -> Option<AckKey> {
    if !step.auto || step.msg.cseq.is_none() || step.retransmits.is_none_or(|n| n == 0) {
        return None;
    }
    match message {
        SipMessage::Request(r) => ack_key_of_ack(r),
        SipMessage::Response(_) => None,
    }
}

/// Where a response to `request` goes: the topmost Via's sent-by, per RFC 3261
/// §18.2.2.
fn via_target(request: &sip_message::SipRequest) -> Option<std::net::SocketAddr> {
    let via = request.top_via();
    let (host, port) = via.host_port();
    let host = via.received().unwrap_or(host);
    format!("{host}:{port}").parse().ok()
}

/// Hold a preserved emission to the block the document stores (§11): a
/// `verbatim-emission` or `raw-order` step is VERIFIED against the message that
/// was composed, so the property is a guarantee and not an intention.
fn preserved_block(
    step: &CompiledStep,
    effects: &StepEffects,
    rendered: &render::Rendered,
    message: &SipMessage,
) -> Result<(), Failure> {
    let Some(deviation) = effects.preserve_stored_block.first() else { return Ok(()) };
    preserve::verify(rendered.template.headers(), message.headers()).map_err(|detail| {
        Failure::EmissionNotPreserved {
            step: step.id.clone(),
            leg: step.leg.clone(),
            deviation: deviation.clone(),
            detail,
        }
    })
}

/// The step outcome a message contributes to the `${step:…}` namespace.
fn outcome_of(inbound: &Inbound) -> StepOutcome {
    StepOutcome {
        status: inbound.status,
        cseq: Some(inbound.cseq),
        cseq_method: Some(inbound.cseq_method.clone()),
        rseq: inbound.rseq,
        method: inbound.method.clone(),
        headers: inbound.headers.clone(),
    }
}

/// The reason phrase a status carries when the document states none. A document
/// that states one wins; this is only the fallback for a status the flow
/// invented.
fn reason_for(status: u16) -> &'static str {
    match status {
        100 => "Trying",
        180 => "Ringing",
        183 => "Session Progress",
        200 => "OK",
        202 => "Accepted",
        404 => "Not Found",
        480 => "Temporarily Unavailable",
        486 => "Busy Here",
        487 => "Request Terminated",
        488 => "Not Acceptable Here",
        491 => "Request Pending",
        603 => "Decline",
        _ => "Response",
    }
}
