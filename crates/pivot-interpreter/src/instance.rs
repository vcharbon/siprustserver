//! One **run instance**: an immutable plan plus this run's identity
//! substitution (`PCAP2TEST_PIVOT_V3.md` §14, "compile once, run many").
//!
//! The plan is shared and never mutated. Everything that differs between two
//! runs of one document — the numbers the lane bound, the Call-IDs and tags the
//! stack minted, the branch an `alt` took, the datagrams that crossed the wire —
//! lives here and is discarded with the run.
//!
//! Each instance mints a NONCE the stack folds into every dialog identity, so
//! two instances of one plan never collide (§14, "compile once, run many").

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

use pivot_schema::bundle::{
    Abandoned, CheckDisposition, Failure, Informative, RetransmitNote, RunConfig, RunVerdict,
    TimingNote, Waived,
};
use pivot_schema::known_bug::KnownBug;
use pivot_schema::placement::ClaimBy;

use crate::background::{Background, Policy};
use crate::claim::{Candidate, ClaimError, ClaimIndex};
use crate::cursor::Cursor;
use crate::gate::Inbound;
use crate::plan::Plan;
use crate::recording::Recording;
use crate::scope::{Finding, Scope};
use crate::state::{LegState, RunState, StepOutcome};

/// One run of one plan.
pub struct Instance<'p> {
    plan: &'p Plan,
    config: RunConfig,
    /// This instance's dialog-identity nonce.
    nonce: String,
    state: RunState,
    cursor: Cursor<'p>,
    claims: ClaimIndex,
    background: Background,
    recording: Recording,
    verdict: RunVerdict,
}

impl<'p> Instance<'p> {
    /// Bind `plan` to one lane's configuration. The claim table and the
    /// background policies are derived here, once: a claim's numbers come from
    /// the lane's binding of the leg's own callee identity, never from a number
    /// the document holds.
    #[cfg(test)]
    pub fn new(plan: &'p Plan, config: RunConfig) -> Self {
        Instance::with_recording(plan, config, Recording::new())
    }

    /// An instance recording into a handle the CALLER already holds — the
    /// pattern that keeps a run's evidence when the run body unwinds.
    pub fn with_recording(plan: &'p Plan, config: RunConfig, recording: Recording) -> Self {
        let candidates = claim_candidates(plan, &config);
        let mut verdict = RunVerdict::ok(plan.document().case.id.clone(), config.lane.clone());
        // A claim whose number set is empty matches nothing, so the run would
        // dial and then report an unclaimed arrival. Say the real cause here.
        for candidate in &candidates {
            if candidate.by == ClaimBy::RuriPos && candidate.numbers.is_empty() {
                verdict.fail(Failure::IdentityUnbound {
                    site: format!("leg {:?} claims by ruri-pos", candidate.leg),
                    identity: callee_identity(plan, &candidate.leg).unwrap_or_default(),
                    detail: unbound_detail(plan, &config, &candidate.leg),
                });
            }
        }
        // A per-call directive that reaches no dial is a lane statement with no
        // effect (§4.3): a mistyped call id, or a call this vantage only
        // witnesses being dialled. Either way the egress it states never
        // happens, so the run says so instead of running a different test.
        for call in config.call_headers.keys() {
            let known = plan.document().calls.iter().any(|c| &c.id == call);
            let detail = if !known {
                "no calls[] entry carries that id".to_string()
            } else if plan.dial_of_call(call).is_none() {
                "this vantage sends no INVITE on that call's caller leg".to_string()
            } else {
                continue;
            };
            verdict.fail(Failure::CallDirectiveUnplaced { call: call.clone(), detail });
        }
        // Every declared violation is LISTED before the run dials, so the
        // bundle names what the case reproduces whatever the run then does.
        for violation in &plan.document().rfc_violations {
            verdict.note_violation(violation);
        }
        let nonce = config.identity_nonce.clone().unwrap_or_else(mint_nonce);
        Instance {
            plan,
            config,
            nonce,
            state: RunState::new(),
            cursor: Cursor::new(plan),
            claims: ClaimIndex::new(candidates),
            background: Background::new(background_policies(plan)),
            recording,
            verdict,
        }
    }

    /// This instance's dialog-identity nonce: what keeps two runs of one plan
    /// from sharing a Call-ID or a From-tag.
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub fn plan(&self) -> &'p Plan {
        self.plan
    }

    pub fn config(&self) -> &RunConfig {
        &self.config
    }

    pub fn state(&self) -> &RunState {
        &self.state
    }

    pub fn cursor(&self) -> &Cursor<'p> {
        &self.cursor
    }

    pub fn background(&self) -> &Background {
        &self.background
    }

    /// The recording handle. Cloned OUT of the instance before the run body, so
    /// a panicking run still leaves its ladder on disk.
    pub fn recording(&self) -> Recording {
        self.recording.clone()
    }

    pub fn verdict(&self) -> &RunVerdict {
        &self.verdict
    }

    /// Record a gating failure — the one door every failure passes through,
    /// routed like any finding, with no class to downgrade it.
    pub fn fail(&mut self, failure: Failure) {
        self.record(Finding::gating(failure));
    }

    /// State where an abandoned script stopped and what ran in its place
    /// (§11.2).
    pub fn note_abandoned(&mut self, abandoned: Abandoned) {
        self.verdict.abandoned = Some(abandoned);
    }

    /// Record a timer-anchored dwell reading (§9.2).
    pub fn note_timing(&mut self, note: TimingNote) {
        self.verdict.timings.push(note);
    }

    /// State every step's retransmit-ladder reading (§6.9), once, at seal time.
    pub fn note_retransmits(&mut self, notes: Vec<RetransmitNote>) {
        self.verdict.retransmits = notes;
    }

    /// Mark a step done. Returns the `optional` steps its completion released.
    pub fn complete_step(&mut self, step: &str) -> Vec<String> {
        self.cursor.complete(step)
    }

    /// Release a tolerated absence; refused on anything that is not an
    /// `optional` expect.
    pub fn release_step(&mut self, step: &str) -> bool {
        self.cursor.release(step)
    }

    /// Retire a required expect a final on its own transaction made
    /// unsatisfiable; refused on anything else.
    pub fn retire_step(&mut self, step: &str) -> bool {
        self.cursor.retire(step)
    }

    /// Deliver an inbound INVITE to the claim table: which leg owns it.
    pub fn claim(
        &mut self,
        endpoint: &str,
        ruri_user: &str,
        inbound: &Inbound,
    ) -> Result<String, ClaimError> {
        self.claims.claim(endpoint, ruri_user, inbound)
    }

    /// Count a message a background policy answered.
    pub fn note_background_answered(&mut self, index: usize) {
        self.background.note_answered(index);
    }

    /// One leg's dialog state, created on first touch — the publish point for
    /// the facts the stack learns and `${leg:…}` resolves against.
    pub fn leg_mut(&mut self, leg: &str) -> &mut LegState {
        self.state.leg_mut(leg)
    }

    /// Publish the tag one `early` id rides: an answered fork's before the run
    /// speaks, an observed fork's the moment the run learns it.
    pub fn mint_early(&mut self, id: &str, leg: &str, tag: &str) {
        self.state.mint_early(id, leg, tag);
    }

    /// The RSeq a reliable provisional put on one fork.
    pub fn record_early_rseq(&mut self, id: &str, rseq: u32) {
        self.state.record_early_rseq(id, rseq);
    }

    /// Record what a step's message carried; a retransmission never overwrites
    /// the first sighting.
    pub fn record_step(&mut self, step: &str, outcome: StepOutcome) {
        self.state.record_step(step, outcome);
    }

    /// This run's lane scoping (§9.1): the lane's configuration against the
    /// document's origin lane.
    pub fn scope(&self) -> Scope<'_> {
        Scope::new(&self.config, self.plan.document().case.origin_lane.as_deref())
    }

    /// Route a check finding by its class, and answer whether it GATED.
    ///
    /// A gating finding fails the run; a classified one the lane does not gate
    /// on lands in the verdict's `informative` section with its class, and the
    /// run's status stays a statement about gating checks alone.
    pub fn record(&mut self, finding: Finding) -> bool {
        let informative = finding.class.filter(|class| {
            self.scope().disposition(Some(*class)) == CheckDisposition::Informative
        });
        match informative {
            Some(class) => {
                self.verdict.informative.push(Informative { class, finding: finding.failure });
                false
            }
            None => {
                self.verdict.fail(finding.failure);
                true
            }
        }
    }

    /// Record a gate the lane's declared known bug stood down. It never fails
    /// the run — that is what declaring the bug bought — but the verdict says
    /// the match was bought, so a reader can tell it from a clean one.
    pub fn record_waived(&mut self, bug: KnownBug, finding: Failure) {
        self.verdict.waived.push(Waived { bug, finding });
    }

    /// Fold the cursor's own record into the verdict — the branches it
    /// committed, the steps it completed, the tolerated absences it released —
    /// and then decide a NEGATIVE document's outcome against what it observed.
    ///
    /// The inversion runs LAST and once, with the whole recording in: a
    /// declaration is matched against the datagrams that crossed the wire, and
    /// nothing can still be arriving by the time it is read (§11.2).
    pub fn seal_verdict(&mut self) {
        for item in &self.plan.program().items {
            if let Some(branch) = self.cursor.committed_branch(&item.id) {
                self.verdict.branches.insert(item.id.clone(), branch.to_string());
            }
        }
        self.verdict.completed_steps = self.cursor.completed().to_vec();
        self.verdict.released_optional = self.cursor.released().to_vec();
        self.verdict.retired = self.cursor.retired().to_vec();
        crate::must_fail::invert(&mut self.verdict, self.plan, &self.recording);
    }
}

/// One claim candidate per leg an actor RECEIVES on, in document order.
fn claim_candidates(plan: &Plan, config: &RunConfig) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (order, leg) in plan.document().legs.iter().enumerate() {
        if leg.dir != pivot_schema::placement::Direction::In {
            continue;
        }
        let Some(actor) = plan.actor(&leg.actor) else { continue };
        // A receiving actor the document leaves claim-less falls back to arrival
        // order — the last-resort rule, stated rather than guessed at.
        let by = actor.claim.map(|c| c.by).unwrap_or(ClaimBy::ArrivalOrder);
        let numbers = callee_numbers(plan, config, &leg.id);
        out.push(Candidate {
            leg: leg.id.clone(),
            actor: actor.id.clone(),
            endpoint: actor.endpoint.clone(),
            by,
            numbers,
            order,
        });
    }
    out
}

/// A fresh dialog-identity nonce. Wall time plus a process counter: the counter
/// separates instances inside one process, the timestamp separates processes and
/// reruns — and neither reads `tokio::time`, which a paused test rewinds.
pub fn mint_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(seq);
    format!("{stamp:x}{seq:x}")
}

/// The identity an attempt on `leg` dials.
fn callee_identity(plan: &Plan, leg: &str) -> Option<String> {
    plan.document()
        .calls
        .iter()
        .flat_map(|call| call.attempts.iter())
        .find(|attempt| attempt.leg == leg)
        .map(|attempt| attempt.callee.identity.clone())
}

/// Why a leg's number set came out empty, in the lane binding's own words.
fn unbound_detail(plan: &Plan, config: &RunConfig, leg: &str) -> String {
    let Some(name) = callee_identity(plan, leg) else {
        return "no attempt on this leg names a callee identity".into();
    };
    let forms = plan.forms(&name).cloned().unwrap_or_default();
    if forms.is_empty() {
        return format!("identity {name:?} declares no dial form to bind");
    }
    forms
        .iter()
        .map(|form| match config.identities.resolve(&name, form) {
            Ok(_) => format!("{form}: bound"),
            Err(e) => format!("{form}: {e}"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Every number the lane bound for the callee identity of `leg`, over every
/// dial form the identity declares.
fn callee_numbers(plan: &Plan, config: &RunConfig, leg: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for call in &plan.document().calls {
        for attempt in call.attempts.iter().filter(|a| a.leg == leg) {
            let name = &attempt.callee.identity;
            // What the lane says the system egresses this leg under wins where
            // it says anything: a lane that dials one form and relays another
            // is the only party that knows which the claim will see.
            if let Some(users) = config.claim_numbers.get(name) {
                out.extend(users.iter().cloned());
            }
            let forms = plan.forms(name).cloned().unwrap_or_default();
            for form in &forms {
                if let Ok(number) = config.identities.resolve(name, form) {
                    out.insert(number.to_string());
                }
            }
        }
    }
    out
}

/// Every actor's background policies, flattened.
fn background_policies(plan: &Plan) -> Vec<Policy> {
    plan.document()
        .actors
        .iter()
        .flat_map(|actor| {
            actor.background.iter().map(move |policy| Policy {
                actor: actor.id.clone(),
                method: policy.r#match.method.clone(),
                status: policy.respond.status,
                count: policy.count,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
