//! The **executor**: a plan instance driven over the scenario harness.
//!
//! The whole job (`PCAP2TEST_PIVOT_V3.md` §14) and nothing beside it: bind the
//! endpoints the lane supplies, sequence the flow through
//! [`Cursor`](crate::cursor::Cursor), emit what a `send` states through [`LegStack`], gate an
//! `expect` through [`gate`](crate::gate), answer `background` without moving the cursor, record
//! every datagram verbatim, settle, and evaluate `postconditions`.
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

mod arbitration;
mod completion;
mod compose;
mod deliver;
mod dispatch;
mod emit;
mod flow;
mod ladders;
mod legs;
mod policy;
mod script_end;
mod settling;
mod stall;
mod unscripted;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use pivot_schema::bundle::{Abandoned, Dir, Failure, RunConfig, RunTiming, RunVerdict};
use scenario_harness::absorption::{Absorption, SeenBy, WireEntry};
use scenario_harness::dialogs::BackwardsCseq;
use scenario_harness::Agent;
use sip_message::{SipMessage, SipResponse};
use tokio::time::Instant;

use crate::early::{EarlyDialogs, LearnedForks};
use crate::gate::Inbound;
use crate::instance::Instance;
use crate::plan::Plan;
use crate::recording::Recording;
use crate::render::UriComposer;
use crate::retransmit::{DrawnAcks, Repeats};
use crate::settle::Sut;
use crate::stack::LegStack;

/// What the lane supplies: a bound agent per actor, where a request goes, and
/// how a tier-2 position becomes a URI.
pub struct Lane<'a> {
    /// Actor id → the agent that plays it.
    pub agents: BTreeMap<String, Agent>,
    /// The system under test's ingress: where an out-of-dialog request goes.
    pub route_target: std::net::SocketAddr,
    /// The lane's media plane: the address an SDP body's connection line is
    /// rewritten to and the port book its media lines draw from, or the
    /// verbatim booking of a lane without media.
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
    /// view it belongs to — the WIRE view of the wire/TU pair. The recording is
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
    /// `(leg, CSeq)` of every non-2xx INVITE final whose Timer H the settle has
    /// already reported expired: the failure is stated once, not once per turn.
    timer_h_reported: std::collections::BTreeSet<(String, u32)>,
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
        // It owns the dialog verdicts too: each UA's table judges what it
        // sights, and the run reads the verdict where it dispatches.
        for agent in lane.agents.values() {
            agent.drop_to_raw_wire();
            agent.hand_dialogs_to_driver(BackwardsCseq::Accept);
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
            timer_h_reported: std::collections::BTreeSet::new(),
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
        RunTiming { started_at_ms: 0, settled_at_ms: settled_at, settle_budget_ms: settle_budget }
    }

    /// Record one ARRIVING datagram, with the §17.2 seam deciding whether it is
    /// a repeat: a repeat carries its `repeat_of` back-reference to the datagram
    /// it repeats, everything else does not. The classification is
    /// the seam's alone — this only chooses which recording door to use.
    fn record_arrival(
        &self,
        leg: &str,
        wire: Vec<u8>,
        step: Option<&str>,
        note: Option<&str>,
        repeat: bool,
    ) {
        let recording = self.instance.recording();
        let at_us = self.now_us();
        if repeat {
            recording.push_repeat(leg, Dir::In, at_us, wire, step, note);
        } else {
            recording.push(leg, Dir::In, at_us, wire, step, note);
        }
    }
}
