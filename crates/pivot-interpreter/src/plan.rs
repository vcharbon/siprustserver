//! Document → **immutable plan** (`PCAP2TEST_PIVOT_V3.md` §14, "compile once,
//! run many").
//!
//! Compilation resolves every reference the document makes — leg, actor,
//! endpoint, identity, step id, delay anchor, `after`, deviation target,
//! accessor — and states each refusal it finds as its own [`PlanError`]
//! variant. Nothing is defaulted into a pass: a step that names no
//! discriminator, an anchor pointing forward, an accessor naming a step inside
//! an `alt` branch it is not part of, a `cseq-override` with no value are all
//! compilation FAILURES, surfaced in the run bundle rather than tolerated.
//!
//! The plan is shared across call instances and never mutated; per-call
//! identity substitution rides [`crate::instance::Instance`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pivot_schema::accessor::{Accessor, StepField};
use pivot_schema::body::Body;
use pivot_schema::check::{Check, CheckOp};
use pivot_schema::deviation::{CseqValue, Deviation};
use pivot_schema::document::PivotV3;
use pivot_schema::flow::{Anchor, CheckMode, Delay, FlowNode, Op, Step};
use pivot_schema::msg::{MsgSpec, Ref};
use pivot_schema::placement::{Actor, Endpoint, Leg};
use pivot_schema::postcondition::CdrExpectation;

use crate::program::{Branch, Item, ItemKind, Program, StepLoc};

/// Why a document cannot be compiled to a plan. Every variant names the site it
/// refuses and what is wrong with it: a compilation refusal is reported to the
/// author, never absorbed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The document declares a version this interpreter does not model.
    Version { found: u32, expected: u32 },
    /// Two objects claim one id.
    DuplicateId { id: String, first: String, second: String },
    /// A step rides a leg the document does not declare.
    UnknownLeg { step: String, leg: String },
    /// A leg names an actor the document does not declare.
    UnknownActor { leg: String, actor: String },
    /// An actor names an endpoint the document does not declare.
    UnknownEndpoint { actor: String, endpoint: String },
    /// An attempt, an actor or an accessor names an identity the registry does
    /// not hold.
    UnknownIdentity { site: String, name: String },
    /// A call names a caller leg the document does not declare.
    UnknownCallerLeg { call: String, leg: String },
    /// A step states neither a method nor a status, so nothing discriminates it.
    NoDiscriminator { step: String },
    /// A step states both a method and a status.
    BothMethodAndStatus { step: String },
    /// An `expect` states no `check`: whether its stored content is matched or
    /// merely recorded is the document's to say, never the interpreter's.
    ExpectWithoutCheckMode { step: String },
    /// A `send` states a field only an `expect` may carry.
    SendCarriesExpectField { step: String, field: String },
    /// A delay anchor, an `after`, a deviation or a marker names an id nothing
    /// declares.
    UnknownReference { site: String, reference: String },
    /// A reference points forward: what it names has not run when it is read.
    ForwardReference { site: String, reference: String },
    /// A reference crosses an `alt` branch boundary (§6.5).
    CrossBranchReference { site: String, reference: String },
    /// A `${…}` that is not an accessor.
    MalformedAccessor { site: String, text: String, detail: String },
    /// `${step:<id>.branch}` on a node that is not an `alt`.
    BranchAccessorOnNonAlt { site: String, step: String },
    /// `${num:<name>:<form>}` in a form the identity does not declare.
    UndeclaredDialForm { site: String, name: String, form: String },
    /// A deviation whose kind has a defined payload states none.
    DeviationMissingPayload { deviation: String, kind: String, missing: String },
    /// A `suppress-auto` naming a step the interpreter never composes.
    SuppressesScriptedStep { deviation: String, step: String },
    /// A deviation that names neither a step nor a leg applies to nothing.
    DeviationTargetsNothing { deviation: String, kind: String },
    /// An `alt` with fewer than two branches.
    AltTooFewBranches { alt: String, branches: usize },
    /// An `alt` branch with no steps: nothing can commit to it.
    AltEmptyBranch { alt: String, branch: String },
    /// An `alt` branch opening on a `send`: which branch runs is decided by what
    /// ARRIVES.
    AltBranchOpensOnSend { alt: String, branch: String, step: String },
    /// An `alt` branch opening on an `optional` expect: an absence cannot commit.
    AltBranchOpensOnOptional { alt: String, branch: String, step: String },
    /// Two `alt` branches whose first messages share one discriminator.
    AltIndiscriminable { alt: String, left: String, right: String, discriminator: String },
    /// Two `alt` branches sharing a name.
    AltDuplicateBranchName { alt: String, name: String },
    /// An `unordered` group with fewer than two steps.
    UnorderedTooFewSteps { group: String, steps: usize },
    /// An `unordered` group holding a `send`: the runner controls when it sends,
    /// so an order-free group holds only what it WAITS for.
    UnorderedHoldsASend { group: String, step: String },
    /// A reference between two members of one `unordered` group: an order-free
    /// group has no internal order to be earlier in.
    UnorderedInternalReference { group: String, from: String, to: String },
    /// A check whose `op` and `value` disagree.
    CheckValueMismatch { site: String, op: String, detail: String },
    /// A `regex` check whose pattern does not compile.
    CheckRegexInvalid { site: String, pattern: String, detail: String },
    /// A dwell that is both timer-linked and compressible: compressing what a
    /// system timer measures changes what the test proves.
    CompressibleTimerLinkedDwell { step: String },
    /// An `early` id on a request `send` that does not RIDE an early dialog.
    /// PRACK (RFC 3262 §7.2) and UPDATE (RFC 3311 §5.1) run inside one before
    /// it confirms; every other request waits for a confirmed dialog, so naming
    /// a fork on one says nothing.
    EarlyDialogOnRequestSend { step: String, early: String },
    /// An `early` id neither ANSWERED by a response `send` on its leg nor
    /// OBSERVED by a response `expect` on it. Such a dialog has no To-tag: none
    /// this run mints, and none it can learn.
    EarlyDialogUnestablished { step: String, leg: String, early: String },
    /// An `early` id both ANSWERED by a `send` and OBSERVED by an `expect` on
    /// one leg. One fork is one side's dialog, so a name both sides claim names
    /// two dialogs.
    EarlyDialogAnsweredAndObserved { leg: String, early: String },
    /// An `${early:…}` naming an id no step declares.
    UnknownEarlyDialog { site: String, early: String },
    /// An `${early:…}` naming an id two legs declare. A leg owns its own fork
    /// tag space, so one id on two legs is two dialogs and the accessor names
    /// neither.
    AmbiguousEarlyDialog { site: String, early: String },
    /// A reliable provisional `send` (RFC 3262 §3: `Require: 100rel`) stating no
    /// `RSeq`. The interpreter never invents a sequence number the peer
    /// acknowledges with `RAck`.
    ReliableProvisionalWithoutRSeq { step: String },
    /// An `overlap` naming a step on another leg. A race is two steps ARMED
    /// TOGETHER on one leg's frontier; legs already run independently, so
    /// declaring one across them says nothing the document does not already say.
    OverlapCrossesLegs { step: String, overlap: String, leg: String, other: String },
    /// An `overlap` naming a step that is not this one's immediate neighbour on
    /// their leg. Arming the two together would skip whatever sits between them,
    /// which is an order the document states and the race does not revoke.
    OverlapNotAdjacent { step: String, overlap: String },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Version { found, expected } => {
                write!(f, "pivot_version {found} is not {expected}")
            }
            PlanError::DuplicateId { id, first, second } => {
                write!(f, "id {id:?} is claimed by both {first} and {second}")
            }
            PlanError::UnknownLeg { step, leg } => {
                write!(f, "step {step:?} rides leg {leg:?}, which is not declared")
            }
            PlanError::UnknownActor { leg, actor } => {
                write!(f, "leg {leg:?} names actor {actor:?}, which is not declared")
            }
            PlanError::UnknownEndpoint { actor, endpoint } => {
                write!(f, "actor {actor:?} names endpoint {endpoint:?}, which is not declared")
            }
            PlanError::UnknownIdentity { site, name } => {
                write!(f, "{site} names identity {name:?}, which the registry does not hold")
            }
            PlanError::UnknownCallerLeg { call, leg } => {
                write!(f, "call {call:?} names caller leg {leg:?}, which is not declared")
            }
            PlanError::NoDiscriminator { step } => {
                write!(f, "step {step:?} states neither a method nor a status")
            }
            PlanError::BothMethodAndStatus { step } => {
                write!(f, "step {step:?} states both a method and a status")
            }
            PlanError::ExpectWithoutCheckMode { step } => {
                write!(f, "expect {step:?} states no `check` (assert or record)")
            }
            PlanError::SendCarriesExpectField { step, field } => {
                write!(f, "send {step:?} carries {field:?}, which only an expect may state")
            }
            PlanError::UnknownReference { site, reference } => {
                write!(f, "{site} names {reference:?}, which nothing declares")
            }
            PlanError::ForwardReference { site, reference } => {
                write!(f, "{site} names {reference:?}, which has not run when it is read")
            }
            PlanError::CrossBranchReference { site, reference } => {
                write!(f, "{site} names {reference:?}, inside an alt branch it is not part of")
            }
            PlanError::MalformedAccessor { site, text, detail } => {
                write!(f, "{site} carries {text:?}: {detail}")
            }
            PlanError::BranchAccessorOnNonAlt { site, step } => {
                write!(f, "{site} reads `.branch` of {step:?}, which is not an alt")
            }
            PlanError::UndeclaredDialForm { site, name, form } => {
                write!(f, "{site} asks identity {name:?} for form {form:?}, which it does not declare")
            }
            PlanError::DeviationMissingPayload { deviation, kind, missing } => {
                write!(f, "deviation {deviation:?} of kind {kind:?} states no {missing}")
            }
            PlanError::SuppressesScriptedStep { deviation, step } => {
                write!(f, "deviation {deviation:?} suppresses {step:?}, which is not an auto step")
            }
            PlanError::DeviationTargetsNothing { deviation, kind } => {
                write!(f, "deviation {deviation:?} of kind {kind:?} names neither a step nor a leg")
            }
            PlanError::AltTooFewBranches { alt, branches } => {
                write!(f, "alt {alt:?} declares {branches} branch(es); two is the minimum")
            }
            PlanError::AltEmptyBranch { alt, branch } => {
                write!(f, "alt {alt:?} branch {branch:?} holds no step")
            }
            PlanError::AltBranchOpensOnSend { alt, branch, step } => {
                write!(f, "alt {alt:?} branch {branch:?} opens on send {step:?}")
            }
            PlanError::AltBranchOpensOnOptional { alt, branch, step } => {
                write!(f, "alt {alt:?} branch {branch:?} opens on optional expect {step:?}")
            }
            PlanError::AltIndiscriminable { alt, left, right, discriminator } => {
                write!(f, "alt {alt:?} branches {left:?} and {right:?} both open on {discriminator}")
            }
            PlanError::AltDuplicateBranchName { alt, name } => {
                write!(f, "alt {alt:?} declares branch {name:?} twice")
            }
            PlanError::UnorderedTooFewSteps { group, steps } => {
                write!(f, "unordered {group:?} holds {steps} step(s); two is the minimum")
            }
            PlanError::UnorderedHoldsASend { group, step } => {
                write!(f, "unordered {group:?} holds send {step:?}")
            }
            PlanError::UnorderedInternalReference { group, from, to } => {
                write!(f, "unordered {group:?}: {from:?} references {to:?}, its own group member")
            }
            PlanError::CheckValueMismatch { site, op, detail } => {
                write!(f, "{site}: check op {op:?} {detail}")
            }
            PlanError::CheckRegexInvalid { site, pattern, detail } => {
                write!(f, "{site}: regex {pattern:?} does not compile: {detail}")
            }
            PlanError::EarlyDialogOnRequestSend { step, early } => write!(
                f,
                "send {step:?} rides early dialog {early:?}; §6.1 names a fork on a response \
                 that answers under it, or on the PRACK or UPDATE that runs inside it, and this \
                 request is neither"
            ),
            PlanError::EarlyDialogUnestablished { step, leg, early } => write!(
                f,
                "step {step:?} rides early dialog {early:?}, which nothing on leg {leg:?} \
                 establishes; a fork is established by the response send that ANSWERS under it \
                 (this run mints the To-tag) or by a response expect that OBSERVES it (the run \
                 learns the peer's To-tag from the first arrival)"
            ),
            PlanError::EarlyDialogAnsweredAndObserved { leg, early } => write!(
                f,
                "early dialog {early:?} on leg {leg:?} is both answered by a send and observed \
                 by an expect; one fork is one side's dialog, so a name both sides claim names \
                 two dialogs — give each its own id"
            ),
            PlanError::UnknownEarlyDialog { site, early } => {
                write!(f, "{site}: early dialog {early:?} is declared by no step")
            }
            PlanError::AmbiguousEarlyDialog { site, early } => write!(
                f,
                "{site}: early dialog {early:?} is declared on two legs, so it names two dialogs"
            ),
            PlanError::ReliableProvisionalWithoutRSeq { step } => write!(
                f,
                "send {step:?} requires 100rel and states no RSeq; RFC 3262 §3 puts one on \
                 every reliable provisional and the interpreter invents none"
            ),
            PlanError::CompressibleTimerLinkedDwell { step } => {
                write!(f, "step {step:?} states a dwell that is both timer_linked and compressible")
            }
            PlanError::OverlapCrossesLegs { step, overlap, leg, other } => write!(
                f,
                "step {step:?} on leg {leg:?} declares an overlap with {overlap:?} on leg \
                 {other:?}; a race is armed on ONE leg's frontier"
            ),
            PlanError::OverlapNotAdjacent { step, overlap } => write!(
                f,
                "step {step:?} declares an overlap with {overlap:?}, which is not its \
                 neighbour on their leg"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// What tells an inbound message from every other message on its leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discriminator {
    /// A request, by method.
    Request { method: String },
    /// A response, by status and — where the document states it — the CSeq
    /// method of the transaction it answers.
    Response { status: u16, cseq_method: Option<String> },
}

impl std::fmt::Display for Discriminator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Discriminator::Request { method } => write!(f, "{method}"),
            Discriminator::Response { status, cseq_method: Some(m) } => write!(f, "{status} to {m}"),
            Discriminator::Response { status, cseq_method: None } => write!(f, "{status}"),
        }
    }
}

impl From<&Discriminator> for pivot_schema::bundle::GatedOn {
    fn from(discriminator: &Discriminator) -> Self {
        match discriminator {
            Discriminator::Request { method } => {
                pivot_schema::bundle::GatedOn::Request { method: method.clone() }
            }
            Discriminator::Response { status, cseq_method } => pivot_schema::bundle::GatedOn::Response {
                status: *status,
                cseq_method: cseq_method.clone(),
            },
        }
    }
}

/// What a compiled step does on the wire, carrying each side's own fields —
/// so an expect's check mode is mandatory here, where the document's `Option`
/// has already been refused if absent, and `optional` cannot exist on a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// The actor emits the message.
    Send,
    /// The actor waits for the message, within its budget.
    Expect {
        /// Whether the stored content is matched or merely recorded.
        check: CheckMode,
        /// Tolerated absence (§6.5).
        optional: bool,
    },
}

/// One flow step, compiled: every field the run needs, resolved once.
#[derive(Debug, Clone)]
pub struct CompiledStep {
    pub id: String,
    pub leg: String,
    pub kind: StepKind,
    pub auto: bool,
    pub after: Vec<String>,
    pub checks: Vec<Check>,
    pub early: Option<String>,
    pub overlap: Option<String>,
    pub msg: MsgSpec,
    pub delay: Delay,
    /// The step's own budget: its `within_ms` where it states one, else
    /// `timing.expect_budget_ms`.
    pub within_ms: u64,
    pub retransmits: Option<u32>,
    /// The document's own pacing for those repeats, one entry per rung. Empty
    /// where the document states none and the RFC's ladder paces instead.
    pub retransmit_intervals_ms: Vec<u64>,
    pub(crate) loc: StepLoc,
    /// Document order, over every step the flow holds.
    pub order: usize,
    /// Indices into `document.deviations` of the entries that apply to this step.
    pub deviations: Vec<usize>,
    pub discriminator: Discriminator,
}

impl CompiledStep {
    /// Whether the run may compress this step's dwell on a virtual clock. Read
    /// from the document and never re-derived.
    pub fn compressible(&self) -> bool {
        self.delay.compressible
    }

    pub fn is_send(&self) -> bool {
        matches!(self.kind, StepKind::Send)
    }

    pub fn is_expect(&self) -> bool {
        matches!(self.kind, StepKind::Expect { .. })
    }

    /// A tolerated absence: an `optional` expect (§6.5).
    pub fn optional_expect(&self) -> bool {
        matches!(self.kind, StepKind::Expect { optional: true, .. })
    }

    /// Whether the stored content is MATCHED against the inbound message
    /// (`check: assert`) rather than merely recorded.
    pub fn asserts_content(&self) -> bool {
        matches!(self.kind, StepKind::Expect { check: CheckMode::Assert, .. })
    }
}

/// A document compiled: immutable, shared across call instances.
#[derive(Debug, Clone)]
pub struct Plan {
    document: Arc<PivotV3>,
    program: Program,
    steps: BTreeMap<String, CompiledStep>,
    legs: BTreeMap<String, Leg>,
    actors: BTreeMap<String, Actor>,
    endpoints: BTreeMap<String, Endpoint>,
    /// Leg id → the actor that plays it, resolved once.
    actor_of_leg: BTreeMap<String, String>,
    /// Leg id → the call it belongs to, from `caller_leg` and `attempts[].leg`.
    call_of_leg: BTreeMap<String, String>,
    /// Call id → the step that DIALS it: the first INVITE this vantage sends on
    /// the call's own caller leg. A call whose opening INVITE is only WITNESSED
    /// — a callee-side vantage — has no entry, because this run dials nothing
    /// for it.
    dial_of_call: BTreeMap<String, String>,
    /// Identity name → the dial forms it declares.
    forms: BTreeMap<String, BTreeSet<String>>,
}

impl Plan {
    /// Compile `document`, or state every reason it cannot be compiled.
    ///
    /// The refusals are collected rather than short-circuited: an author fixing
    /// a document sees the whole list, and a run bundle records all of it.
    pub fn compile(document: PivotV3) -> Result<Plan, Vec<PlanError>> {
        Compiler::new(document).run()
    }

    pub fn document(&self) -> &PivotV3 {
        &self.document
    }

    /// The document, shareable — what the run bundle writes back as `pivot.json`.
    pub fn document_arc(&self) -> Arc<PivotV3> {
        Arc::clone(&self.document)
    }

    pub(crate) fn program(&self) -> &Program {
        &self.program
    }

    pub fn step(&self, id: &str) -> Option<&CompiledStep> {
        self.steps.get(id)
    }

    /// Every compiled step, in document order.
    pub fn steps(&self) -> Vec<&CompiledStep> {
        let mut all: Vec<&CompiledStep> = self.steps.values().collect();
        all.sort_by_key(|s| s.order);
        all
    }

    pub fn leg(&self, id: &str) -> Option<&Leg> {
        self.legs.get(id)
    }

    pub fn legs(&self) -> &BTreeMap<String, Leg> {
        &self.legs
    }

    pub fn actor(&self, id: &str) -> Option<&Actor> {
        self.actors.get(id)
    }

    pub fn actors(&self) -> &BTreeMap<String, Actor> {
        &self.actors
    }

    pub fn endpoint(&self, id: &str) -> Option<&Endpoint> {
        self.endpoints.get(id)
    }

    /// The actor that plays `leg`.
    pub fn actor_of_leg(&self, leg: &str) -> Option<&str> {
        self.actor_of_leg.get(leg).map(String::as_str)
    }

    /// The call `leg` belongs to.
    pub fn call_of_leg(&self, leg: &str) -> Option<&str> {
        self.call_of_leg.get(leg).map(String::as_str)
    }

    /// The step that dials `call` (§4.3): the INVITE that opens its caller leg.
    pub fn dial_of_call(&self, call: &str) -> Option<&str> {
        self.dial_of_call.get(call).map(String::as_str)
    }

    /// The call `step` dials, where `step` is a call's own opening INVITE. This
    /// is what scopes a per-call lane directive to one dial: a re-INVITE later
    /// on the same leg is not a dial, and neither is any step of the neighbour.
    pub fn call_dialled_by(&self, step: &str) -> Option<&str> {
        self.dial_of_call
            .iter()
            .find(|(_, dial)| dial.as_str() == step)
            .map(|(call, _)| call.as_str())
    }

    /// The dial forms identity `name` declares.
    pub fn forms(&self, name: &str) -> Option<&BTreeSet<String>> {
        self.forms.get(name)
    }

    /// The deviations that apply to `step`, as document entries.
    pub fn deviations_for(&self, step: &str) -> Vec<&Deviation> {
        match self.steps.get(step) {
            None => Vec::new(),
            Some(s) => s.deviations.iter().map(|&i| &self.document.deviations[i]).collect(),
        }
    }
}

/// The compilation pass. Collects refusals rather than returning on the first.
struct Compiler {
    document: PivotV3,
    errors: Vec<PlanError>,
    program: Program,
    steps: BTreeMap<String, CompiledStep>,
    order_of: BTreeMap<String, usize>,
    loc_of: BTreeMap<String, StepLoc>,
    node_ids: BTreeSet<String>,
    /// Node id → its item index, for a block; a step id maps to its own item.
    node_order: BTreeMap<String, usize>,
}

impl Compiler {
    fn new(document: PivotV3) -> Self {
        Compiler {
            document,
            errors: Vec::new(),
            program: Program::default(),
            steps: BTreeMap::new(),
            order_of: BTreeMap::new(),
            loc_of: BTreeMap::new(),
            node_ids: BTreeSet::new(),
            node_order: BTreeMap::new(),
        }
    }

    fn run(mut self) -> Result<Plan, Vec<PlanError>> {
        if !self.document.version_matches() {
            self.errors.push(PlanError::Version {
                found: self.document.pivot_version,
                expected: pivot_schema::document::PIVOT_VERSION,
            });
        }
        let (legs, actors, endpoints, actor_of_leg, forms) = self.placement();
        let call_of_leg = self.calls(&legs, &forms);
        self.build_program();
        self.compile_steps();
        self.check_blocks();
        self.check_references(&legs);
        self.check_deviations(&legs);
        self.check_accessors(&legs, &forms);
        self.check_postconditions();
        self.check_violations(&actors);
        self.check_reliable_provisionals();
        let dial_of_call = self.dials();

        if self.errors.is_empty() {
            Ok(Plan {
                document: Arc::new(self.document),
                program: self.program,
                steps: self.steps,
                legs,
                actors,
                endpoints,
                actor_of_leg,
                call_of_leg,
                dial_of_call,
                forms,
            })
        } else {
            Err(self.errors)
        }
    }

    /// Endpoints, actors and legs, each resolved against the one before it.
    #[allow(clippy::type_complexity)]
    fn placement(
        &mut self,
    ) -> (
        BTreeMap<String, Leg>,
        BTreeMap<String, Actor>,
        BTreeMap<String, Endpoint>,
        BTreeMap<String, String>,
        BTreeMap<String, BTreeSet<String>>,
    ) {
        let mut endpoints = BTreeMap::new();
        for e in &self.document.endpoints {
            if endpoints.insert(e.id.clone(), e.clone()).is_some() {
                self.errors.push(PlanError::DuplicateId {
                    id: e.id.clone(),
                    first: "endpoint".into(),
                    second: "endpoint".into(),
                });
            }
        }
        let mut forms: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for i in &self.document.identities {
            if forms.insert(i.name.clone(), i.forms.iter().cloned().collect()).is_some() {
                self.errors.push(PlanError::DuplicateId {
                    id: i.name.clone(),
                    first: "identity".into(),
                    second: "identity".into(),
                });
            }
        }
        let mut actors = BTreeMap::new();
        for a in &self.document.actors {
            if !endpoints.contains_key(&a.endpoint) {
                self.errors.push(PlanError::UnknownEndpoint {
                    actor: a.id.clone(),
                    endpoint: a.endpoint.clone(),
                });
            }
            if let Some(name) = &a.identity {
                if !forms.contains_key(name) {
                    self.errors.push(PlanError::UnknownIdentity {
                        site: format!("actor {:?}", a.id),
                        name: name.clone(),
                    });
                }
            }
            if actors.insert(a.id.clone(), a.clone()).is_some() {
                self.errors.push(PlanError::DuplicateId {
                    id: a.id.clone(),
                    first: "actor".into(),
                    second: "actor".into(),
                });
            }
        }
        let mut legs = BTreeMap::new();
        let mut actor_of_leg = BTreeMap::new();
        for l in &self.document.legs {
            if !actors.contains_key(&l.actor) {
                self.errors
                    .push(PlanError::UnknownActor { leg: l.id.clone(), actor: l.actor.clone() });
            }
            actor_of_leg.insert(l.id.clone(), l.actor.clone());
            if legs.insert(l.id.clone(), l.clone()).is_some() {
                self.errors.push(PlanError::DuplicateId {
                    id: l.id.clone(),
                    first: "leg".into(),
                    second: "leg".into(),
                });
            }
        }
        (legs, actors, endpoints, actor_of_leg, forms)
    }

    /// `calls` is read for leg membership ONLY (§14): which call a leg belongs
    /// to, so a run instance can substitute per call. `cause` and `relay18x`
    /// are never read.
    fn calls(
        &mut self,
        legs: &BTreeMap<String, Leg>,
        forms: &BTreeMap<String, BTreeSet<String>>,
    ) -> BTreeMap<String, String> {
        let mut call_of_leg = BTreeMap::new();
        for c in &self.document.calls {
            if !legs.contains_key(&c.caller_leg) {
                self.errors.push(PlanError::UnknownCallerLeg {
                    call: c.id.clone(),
                    leg: c.caller_leg.clone(),
                });
            }
            call_of_leg.insert(c.caller_leg.clone(), c.id.clone());
            for a in &c.attempts {
                if !legs.contains_key(&a.leg) {
                    self.errors.push(PlanError::UnknownReference {
                        site: format!("call {:?} attempt on leg", c.id),
                        reference: a.leg.clone(),
                    });
                }
                if !forms.contains_key(&a.callee.identity) {
                    self.errors.push(PlanError::UnknownIdentity {
                        site: format!("call {:?} attempt {}/{}", c.id, a.branch, a.position),
                        name: a.callee.identity.clone(),
                    });
                }
                call_of_leg.insert(a.leg.clone(), c.id.clone());
            }
        }
        call_of_leg
    }

    /// Call id → the step that DIALS it (§4.3): the first INVITE this vantage
    /// SENDS on the call's caller leg.
    ///
    /// It is where a per-call lane directive belongs, and only the first one:
    /// the opening INVITE is what a routing decision reads, while a re-INVITE
    /// on the same leg rides an established dialog the decision has already
    /// made. A call this vantage only WITNESSES being dialled has no entry.
    fn dials(&self) -> BTreeMap<String, String> {
        let mut dial_of_call = BTreeMap::new();
        let mut ordered: Vec<&CompiledStep> = self.steps.values().collect();
        ordered.sort_by_key(|s| s.order);
        for call in &self.document.calls {
            if let Some(step) = ordered.iter().find(|s| {
                s.is_send()
                    && s.leg == call.caller_leg
                    && s.msg.method.as_deref() == Some("INVITE")
            }) {
                dial_of_call.insert(call.id.clone(), step.id.clone());
            }
        }
        dial_of_call
    }

    /// The top-level items, their per-leg sequences, and the step locations.
    fn build_program(&mut self) {
        let mut items: Vec<Item> = Vec::new();
        let mut order = 0usize;
        let flow = self.document.flow.clone();
        for (index, node) in flow.iter().enumerate() {
            let mut item = Item {
                index,
                id: node.id().to_string(),
                kind: match node {
                    FlowNode::Message(_) => ItemKind::Message,
                    FlowNode::Inject(_) => ItemKind::Inject,
                    FlowNode::Alt(_) => ItemKind::Alt,
                    FlowNode::Unordered(_) => ItemKind::Unordered,
                },
                legs: BTreeSet::new(),
                after: node.after().to_vec(),
                steps: Vec::new(),
                branches: Vec::new(),
                action: match node {
                    FlowNode::Inject(inject) => Some(inject.action.clone()),
                    _ => None,
                },
            };
            // (step, branch index, index within its list), in document order.
            let placements: Vec<(&Step, Option<usize>, usize)> = match node {
                FlowNode::Message(step) => vec![(step.as_ref(), None, 0)],
                FlowNode::Inject(_) => Vec::new(),
                FlowNode::Alt(alt) => alt
                    .branches
                    .iter()
                    .enumerate()
                    .flat_map(|(bi, branch)| {
                        branch.steps.iter().enumerate().map(move |(si, step)| (step, Some(bi), si))
                    })
                    .collect(),
                FlowNode::Unordered(group) => {
                    group.steps.iter().enumerate().map(|(si, step)| (step, None, si)).collect()
                }
            };
            for (step, branch, within) in placements {
                if !self.node_ids.insert(step.id.clone()) {
                    self.errors.push(PlanError::DuplicateId {
                        id: step.id.clone(),
                        first: "flow node".into(),
                        second: "flow node".into(),
                    });
                }
                self.loc_of.insert(step.id.clone(), StepLoc { item: index, branch, within });
                self.order_of.insert(step.id.clone(), order);
                self.node_order.insert(step.id.clone(), index);
                order += 1;
                item.legs.insert(step.leg.clone());
                item.steps.push(step.id.clone());
            }
            if let FlowNode::Alt(alt) = node {
                item.branches = alt
                    .branches
                    .iter()
                    .map(|b| Branch {
                        name: b.name.clone(),
                        steps: b.steps.iter().map(|s| s.id.clone()).collect(),
                    })
                    .collect();
            }
            if matches!(node, FlowNode::Inject(_) | FlowNode::Alt(_) | FlowNode::Unordered(_))
                && !self.node_ids.insert(item.id.clone())
            {
                self.errors.push(PlanError::DuplicateId {
                    id: item.id.clone(),
                    first: "flow node".into(),
                    second: "flow node".into(),
                });
            }
            self.node_order.insert(item.id.clone(), index);
            items.push(item);
        }
        let mut by_leg: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut item_of_node: BTreeMap<String, usize> = BTreeMap::new();
        for item in &items {
            for leg in &item.legs {
                by_leg.entry(leg.clone()).or_default().push(item.index);
            }
            item_of_node.insert(item.id.clone(), item.index);
            for step in &item.steps {
                item_of_node.insert(step.clone(), item.index);
            }
        }
        self.program = Program { items, by_leg, item_of_node };
    }

    /// One [`CompiledStep`] per flow step, with every per-step refusal stated.
    fn compile_steps(&mut self) {
        let budget = self.document.timing.expect_budget_ms;
        let doc_steps: Vec<Step> = self.document.steps().into_iter().cloned().collect();
        let deviations = self.document.deviations.clone();
        for step in doc_steps {
            let discriminator = match (&step.msg.method, step.msg.status) {
                (Some(_), Some(_)) => {
                    self.errors.push(PlanError::BothMethodAndStatus { step: step.id.clone() });
                    continue;
                }
                (Some(method), None) => Discriminator::Request { method: method.clone() },
                (None, Some(status)) => Discriminator::Response {
                    status,
                    cseq_method: step.msg.cseq_method.clone(),
                },
                (None, None) => {
                    self.errors.push(PlanError::NoDiscriminator { step: step.id.clone() });
                    continue;
                }
            };
            match step.op {
                Op::Expect if step.check.is_none() => {
                    self.errors.push(PlanError::ExpectWithoutCheckMode { step: step.id.clone() });
                }
                Op::Send => {
                    for (present, field) in [
                        (step.check.is_some(), "check"),
                        (step.optional, "optional"),
                        (!step.msg.headers_present.is_empty(), "headers-present"),
                    ] {
                        if present {
                            self.errors.push(PlanError::SendCarriesExpectField {
                                step: step.id.clone(),
                                field: field.into(),
                            });
                        }
                    }
                }
                Op::Expect => {}
            }
            if step.delay.timer_linked && step.delay.compressible {
                self.errors
                    .push(PlanError::CompressibleTimerLinkedDwell { step: step.id.clone() });
            }
            for check in &step.checks {
                if let Err(e) = check_shape(&format!("step {:?} check", step.id), check) {
                    self.errors.push(e);
                }
            }
            let mine: Vec<usize> = deviations
                .iter()
                .enumerate()
                .filter(|(_, d)| d.step.as_deref() == Some(step.id.as_str()))
                .map(|(i, _)| i)
                .collect();
            let Some(&loc) = self.loc_of.get(&step.id) else { continue };
            let order = self.order_of.get(&step.id).copied().unwrap_or_default();
            self.steps.insert(
                step.id.clone(),
                CompiledStep {
                    id: step.id.clone(),
                    leg: step.leg.clone(),
                    kind: match step.op {
                        Op::Send => StepKind::Send,
                        // An absent check mode is refused above; the fallback
                        // never runs, because a refused document compiles to
                        // no plan.
                        Op::Expect => StepKind::Expect {
                            check: step.check.unwrap_or(CheckMode::Record),
                            optional: step.optional,
                        },
                    },
                    auto: step.auto,
                    after: step.after.clone(),
                    checks: step.checks.clone(),
                    early: step.early.clone(),
                    overlap: step.overlap.clone(),
                    msg: step.msg.clone(),
                    delay: step.delay.clone(),
                    within_ms: step.within_ms.unwrap_or(budget),
                    retransmits: step.retransmits,
                    retransmit_intervals_ms: step.retransmit_intervals_ms.clone(),
                    loc,
                    order,
                    deviations: mine,
                    discriminator,
                },
            );
        }
    }

    /// The two reliable-provisional shapes the format states, and the early
    /// dialogs they ring under (RFC 3262 §3, §6.1).
    ///
    /// Every refusal exists so a run never invents dialog state: an RSeq the
    /// document does not carry, a To-tag nothing on the leg establishes, or
    /// one id claiming a dialog on both sides at once.
    fn check_reliable_provisionals(&mut self) {
        let steps: Vec<CompiledStep> = self.steps.values().cloned().collect();
        // An ANSWERED fork's tag is minted by the RESPONSE send that answers
        // under it; an OBSERVED fork's is the peer's, learned from the response
        // expect's first arrival. A request riding the fork consumes a tag; it
        // establishes nothing.
        let answered: BTreeSet<(String, String)> = steps
            .iter()
            .filter(|s| s.is_send() && s.msg.method.is_none())
            .filter_map(|s| s.early.clone().map(|early| (s.leg.clone(), early)))
            .collect();
        let observed: BTreeSet<(String, String)> = steps
            .iter()
            .filter(|s| s.is_expect() && s.msg.method.is_none())
            .filter_map(|s| s.early.clone().map(|early| (s.leg.clone(), early)))
            .collect();
        for (leg, early) in answered.intersection(&observed) {
            self.errors.push(PlanError::EarlyDialogAnsweredAndObserved {
                leg: leg.clone(),
                early: early.clone(),
            });
        }
        for step in &steps {
            if let Some(early) = &step.early {
                let key = (step.leg.clone(), early.clone());
                if step.is_send() && step.msg.method.is_some() && !rides_early_dialog(step) {
                    self.errors.push(PlanError::EarlyDialogOnRequestSend {
                        step: step.id.clone(),
                        early: early.clone(),
                    });
                } else if !answered.contains(&key) && !observed.contains(&key) {
                    self.errors.push(PlanError::EarlyDialogUnestablished {
                        step: step.id.clone(),
                        leg: step.leg.clone(),
                        early: early.clone(),
                    });
                }
            }
            if step.is_send()
                && step.msg.status.is_some_and(|s| (101..200).contains(&s))
                && requires_100rel(&step.msg)
                && !states_header(&step.msg, "RSeq")
            {
                self.errors
                    .push(PlanError::ReliableProvisionalWithoutRSeq { step: step.id.clone() });
            }
        }
    }

    /// `alt` discriminability and `unordered` shape (§6.5): the structural
    /// guarantees the commit-and-never-backtrack rule rests on.
    fn check_blocks(&mut self) {
        let nodes = self.document.flow.clone();
        for node in &nodes {
            match node {
                FlowNode::Alt(alt) => {
                    if alt.branches.len() < 2 {
                        self.errors.push(PlanError::AltTooFewBranches {
                            alt: alt.id.clone(),
                            branches: alt.branches.len(),
                        });
                    }
                    let mut names: BTreeSet<&str> = BTreeSet::new();
                    let mut firsts: Vec<(String, String)> = Vec::new();
                    for branch in &alt.branches {
                        if !names.insert(branch.name.as_str()) {
                            self.errors.push(PlanError::AltDuplicateBranchName {
                                alt: alt.id.clone(),
                                name: branch.name.clone(),
                            });
                        }
                        let Some(first) = branch.steps.first() else {
                            self.errors.push(PlanError::AltEmptyBranch {
                                alt: alt.id.clone(),
                                branch: branch.name.clone(),
                            });
                            continue;
                        };
                        if first.op == Op::Send {
                            self.errors.push(PlanError::AltBranchOpensOnSend {
                                alt: alt.id.clone(),
                                branch: branch.name.clone(),
                                step: first.id.clone(),
                            });
                        }
                        if first.optional {
                            self.errors.push(PlanError::AltBranchOpensOnOptional {
                                alt: alt.id.clone(),
                                branch: branch.name.clone(),
                                step: first.id.clone(),
                            });
                        }
                        let key = match (&first.msg.method, first.msg.status) {
                            (Some(m), _) => format!("{} {m}", first.leg),
                            (None, Some(s)) => format!(
                                "{} {s} {}",
                                first.leg,
                                first.msg.cseq_method.as_deref().unwrap_or("-")
                            ),
                            (None, None) => continue,
                        };
                        if let Some((other, _)) = firsts.iter().find(|(_, k)| *k == key) {
                            self.errors.push(PlanError::AltIndiscriminable {
                                alt: alt.id.clone(),
                                left: other.clone(),
                                right: branch.name.clone(),
                                discriminator: key.clone(),
                            });
                        }
                        firsts.push((branch.name.clone(), key));
                    }
                }
                FlowNode::Unordered(group) => {
                    if group.steps.len() < 2 {
                        self.errors.push(PlanError::UnorderedTooFewSteps {
                            group: group.id.clone(),
                            steps: group.steps.len(),
                        });
                    }
                    for step in &group.steps {
                        if step.op == Op::Send {
                            self.errors.push(PlanError::UnorderedHoldsASend {
                                group: group.id.clone(),
                                step: step.id.clone(),
                            });
                        }
                    }
                }
                FlowNode::Message(_) | FlowNode::Inject(_) => {}
            }
        }
    }

    /// Delay anchors and `after` edges: every one resolves, points backwards,
    /// and stays inside the branch it belongs to.
    fn check_references(&mut self, legs: &BTreeMap<String, Leg>) {
        let steps: Vec<CompiledStep> = self.steps.values().cloned().collect();
        for step in &steps {
            if !legs.contains_key(&step.leg) {
                self.errors.push(PlanError::UnknownLeg {
                    step: step.id.clone(),
                    leg: step.leg.clone(),
                });
            }
            let site = format!("step {:?} delay anchor", step.id);
            if let Anchor::Step(anchor) = &step.delay.from {
                self.reference_holds(&site, anchor, Some(step));
            }
            for after in &step.after {
                self.reference_holds(&format!("step {:?} after", step.id), after, Some(step));
            }
            if let Some(overlap) = &step.overlap {
                let site = format!("step {:?} overlap", step.id);
                self.reference_holds(&site, overlap, Some(step));
                self.overlap_holds(step, overlap);
            }
        }
        let items = self.program.items.clone();
        for item in &items {
            // A message item's `after` IS its step's, already checked above.
            if item.kind == ItemKind::Message {
                continue;
            }
            for after in &item.after {
                let site = format!("node {:?} after", item.id);
                self.block_reference_holds(&site, after, item.index);
            }
        }
        if let Some(defect) = &self.document.case.defect.clone() {
            let marker = defect.marker.step.clone();
            if !self.node_ids.contains(&marker) {
                self.errors.push(PlanError::UnknownReference {
                    site: "case.defect.marker".into(),
                    reference: marker,
                });
            }
        }
    }

    /// A declared race holds only where the interpreter can run it: two steps on
    /// ONE leg, neighbours in that leg's order, armed together on its frontier
    /// (§6.1). A race across legs is nothing — legs already run independently —
    /// and a race over a gap would skip the steps inside it, whose order the
    /// document does state.
    fn overlap_holds(&mut self, step: &CompiledStep, overlap: &str) {
        let Some(other) = self.steps.get(overlap).cloned() else { return };
        if other.leg != step.leg {
            self.errors.push(PlanError::OverlapCrossesLegs {
                step: step.id.clone(),
                overlap: overlap.to_string(),
                leg: step.leg.clone(),
                other: other.leg.clone(),
            });
            return;
        }
        let on_leg = self.program.by_leg.get(&step.leg).cloned().unwrap_or_default();
        let position = |item: usize| on_leg.iter().position(|&i| i == item);
        let adjacent = match (position(step.loc.item), position(other.loc.item)) {
            (Some(a), Some(b)) => a.abs_diff(b) == 1,
            _ => false,
        };
        if !adjacent {
            self.errors.push(PlanError::OverlapNotAdjacent {
                step: step.id.clone(),
                overlap: overlap.to_string(),
            });
        }
    }

    /// One reference from `from` (a step, or a block by index) to `reference`.
    fn reference_holds(&mut self, site: &str, reference: &str, from: Option<&CompiledStep>) {
        if !self.node_ids.contains(reference) {
            self.errors.push(PlanError::UnknownReference {
                site: site.to_string(),
                reference: reference.to_string(),
            });
            return;
        }
        let Some(from) = from else { return };
        let Some(target) = self.loc_of.get(reference).copied() else {
            // A block id: it completes as a whole, and its own position orders it.
            let Some(&target_item) = self.node_order.get(reference) else { return };
            if target_item >= from.loc.item {
                self.errors.push(PlanError::ForwardReference {
                    site: site.to_string(),
                    reference: reference.to_string(),
                });
            }
            return;
        };
        if from.loc.crosses_branch(&target) {
            self.errors.push(PlanError::CrossBranchReference {
                site: site.to_string(),
                reference: reference.to_string(),
            });
            return;
        }
        if from.loc.item == target.item {
            let same_group = from.loc.branch.is_none()
                && target.branch.is_none()
                && self.program.items[from.loc.item].kind == ItemKind::Unordered;
            if same_group {
                self.errors.push(PlanError::UnorderedInternalReference {
                    group: self.program.items[from.loc.item].id.clone(),
                    from: from.id.clone(),
                    to: reference.to_string(),
                });
                return;
            }
            if target.within >= from.loc.within {
                self.errors.push(PlanError::ForwardReference {
                    site: site.to_string(),
                    reference: reference.to_string(),
                });
            }
            return;
        }
        if target.item > from.loc.item {
            self.errors.push(PlanError::ForwardReference {
                site: site.to_string(),
                reference: reference.to_string(),
            });
        }
    }

    /// A block's own `after`: it may not name a step inside an `alt` branch, and
    /// it must point backwards in document order.
    fn block_reference_holds(&mut self, site: &str, reference: &str, from_item: usize) {
        if !self.node_ids.contains(reference) {
            self.errors.push(PlanError::UnknownReference {
                site: site.to_string(),
                reference: reference.to_string(),
            });
            return;
        }
        if let Some(loc) = self.loc_of.get(reference).copied() {
            if loc.branch.is_some() {
                self.errors.push(PlanError::CrossBranchReference {
                    site: site.to_string(),
                    reference: reference.to_string(),
                });
                return;
            }
            if loc.item >= from_item {
                self.errors.push(PlanError::ForwardReference {
                    site: site.to_string(),
                    reference: reference.to_string(),
                });
            }
            return;
        }
        if self.node_order.get(reference).copied().is_some_and(|i| i >= from_item) {
            self.errors.push(PlanError::ForwardReference {
                site: site.to_string(),
                reference: reference.to_string(),
            });
        }
    }

    /// Deviation targets and payloads (§11). `kind` stays open — an unknown kind
    /// COMPILES, so lint and the run bundle can name it; executing one is what
    /// [`crate::deviation`] refuses.
    fn check_deviations(&mut self, legs: &BTreeMap<String, Leg>) {
        let deviations = self.document.deviations.clone();
        for d in &deviations {
            if d.step.is_none() && d.leg.is_none() {
                self.errors.push(PlanError::DeviationTargetsNothing {
                    deviation: d.id.clone(),
                    kind: d.kind.clone(),
                });
            }
            if let Some(leg) = &d.leg {
                if !legs.contains_key(leg) {
                    self.errors.push(PlanError::UnknownReference {
                        site: format!("deviation {:?} leg", d.id),
                        reference: leg.clone(),
                    });
                }
            }
            if let Some(step) = &d.step {
                if !self.node_ids.contains(step) {
                    self.errors.push(PlanError::UnknownReference {
                        site: format!("deviation {:?} step", d.id),
                        reference: step.clone(),
                    });
                }
            }
            match d.kind.as_str() {
                "cseq-override" if d.value.is_none() => {
                    self.errors.push(PlanError::DeviationMissingPayload {
                        deviation: d.id.clone(),
                        kind: d.kind.clone(),
                        missing: "value".into(),
                    });
                }
                "suppress-auto" => match &d.step {
                    None => self.errors.push(PlanError::DeviationMissingPayload {
                        deviation: d.id.clone(),
                        kind: d.kind.clone(),
                        missing: "step".into(),
                    }),
                    Some(step) => {
                        if self.steps.get(step).is_some_and(|s| !s.auto) {
                            self.errors.push(PlanError::SuppressesScriptedStep {
                                deviation: d.id.clone(),
                                step: step.clone(),
                            });
                        }
                    }
                },
                "verbatim-emission" | "raw-order" if d.preserve.is_empty() => {
                    self.errors.push(PlanError::DeviationMissingPayload {
                        deviation: d.id.clone(),
                        kind: d.kind.clone(),
                        missing: "preserve".into(),
                    });
                }
                "malformed-header" if d.header.is_none() => {
                    self.errors.push(PlanError::DeviationMissingPayload {
                        deviation: d.id.clone(),
                        kind: d.kind.clone(),
                        missing: "header".into(),
                    });
                }
                _ => {}
            }
        }
    }

    /// Postcondition checks (§10) hold the same shape rules as an inline one.
    fn check_postconditions(&mut self) {
        let Some(post) = self.document.postconditions.clone() else { return };
        for check in &post.checks {
            if let Err(e) = check_shape("postcondition check", check) {
                self.errors.push(e);
            }
        }
        if let Some(CdrExpectation::Expected(cdr)) = &post.cdr {
            for check in &cdr.checks {
                if let Err(e) = check_shape("postcondition cdr check", check) {
                    self.errors.push(e);
                }
            }
        }
    }

    /// Every `rfc_violations` entry lands on a step of this flow and names an
    /// emitter the document declares. The anchor and the emitter are what the
    /// entry is FOR — one says which datagram breaks the rule, the other decides
    /// whether the run gates on it — so an entry missing either is refused
    /// rather than listed as a note.
    fn check_violations(&mut self, actors: &BTreeMap<String, Actor>) {
        for violation in self.document.rfc_violations.clone() {
            let site = format!("rfc violation {}", violation.rule);
            if !self.steps.contains_key(&violation.step) {
                self.errors.push(PlanError::UnknownReference {
                    site: site.clone(),
                    reference: violation.step.clone(),
                });
            }
            if !violation.sut_emitted() && !actors.contains_key(&violation.emitter) {
                self.errors.push(PlanError::UnknownReference {
                    site,
                    reference: violation.emitter.clone(),
                });
            }
        }
    }

    /// Every `${…}` the document carries, wherever it carries it (§8.1): header
    /// names and values, `headers-present`, tier-2 refs, body refs and content
    /// types, check fields and values, an `inject`'s action and target, and a
    /// `cseq-override`'s relative value.
    fn check_accessors(
        &mut self,
        legs: &BTreeMap<String, Leg>,
        forms: &BTreeMap<String, BTreeSet<String>>,
    ) {
        let early = self.early_legs();
        let sites = self.accessor_sites();
        for (site, from, text) in sites {
            for found in Accessor::scan(&text) {
                match found {
                    Err(detail) => self.errors.push(PlanError::MalformedAccessor {
                        site: site.clone(),
                        text: text.clone(),
                        detail,
                    }),
                    Ok(accessor) => {
                        self.accessor_holds(&site, from.as_deref(), &accessor, legs, forms, &early)
                    }
                }
            }
        }
    }

    /// Every `early` id the flow declares, to the legs that declare it.
    fn early_legs(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for step in self.steps.values() {
            if let Some(early) = &step.early {
                out.entry(early.clone()).or_default().insert(step.leg.clone());
            }
        }
        out
    }

    /// Every string the document carries, with the site that owns it and the
    /// step a backwards reference is measured from.
    fn accessor_sites(&self) -> Vec<(String, Option<String>, String)> {
        let mut out: Vec<(String, Option<String>, String)> = Vec::new();
        for step in self.document.steps() {
            let owner = Some(step.id.clone());
            let site = |what: &str| format!("step {:?} {what}", step.id);
            let msg: &MsgSpec = &step.msg;
            for h in &msg.headers {
                out.push((site("header name"), owner.clone(), h.name.clone()));
                out.push((site(&format!("header {:?}", h.name)), owner.clone(), h.value.clone()));
            }
            for h in &msg.headers_present {
                out.push((site("headers-present"), owner.clone(), h.clone()));
            }
            for (what, r) in
                [("ruri", &msg.ruri), ("from", &msg.from), ("to", &msg.to)]
            {
                let Some(r) = r else { continue };
                match r {
                    Ref::Positional(p) => {
                        out.push((site(what), owner.clone(), p.pos.clone()));
                        if let Some(form) = &p.form {
                            out.push((site(what), owner.clone(), form.clone()));
                        }
                    }
                    Ref::Frozen(f) => {
                        out.push((site(what), owner.clone(), f.frozen.clone()));
                        if let Some(kind) = &f.kind {
                            out.push((site(what), owner.clone(), kind.clone()));
                        }
                    }
                }
            }
            if let Some(body) = &msg.body {
                for text in body_strings(body) {
                    out.push((site("body"), owner.clone(), text));
                }
            }
            for c in &step.checks {
                out.push((site("check field"), owner.clone(), c.field.clone()));
                if let Some(v) = &c.value {
                    out.push((site("check value"), owner.clone(), v.clone()));
                }
            }
        }
        for node in &self.document.flow {
            if let FlowNode::Inject(i) = node {
                out.push((format!("inject {:?} action", i.id), None, i.action.clone()));
                if let Some(t) = &i.target {
                    out.push((format!("inject {:?} target", i.id), None, t.clone()));
                }
            }
        }
        for d in &self.document.deviations {
            if let Some(CseqValue::Relative(c)) = &d.value {
                out.push((
                    format!("deviation {:?} value", d.id),
                    d.step.clone(),
                    c.from.to_string(),
                ));
            }
        }
        if let Some(post) = &self.document.postconditions {
            for c in &post.checks {
                out.push(("postcondition check field".into(), None, c.field.clone()));
                if let Some(v) = &c.value {
                    out.push(("postcondition check value".into(), None, v.clone()));
                }
            }
            if let Some(CdrExpectation::Expected(cdr)) = &post.cdr {
                for c in &cdr.checks {
                    out.push(("postcondition cdr check field".into(), None, c.field.clone()));
                    if let Some(v) = &c.value {
                        out.push(("postcondition cdr check value".into(), None, v.clone()));
                    }
                }
            }
        }
        out
    }

    /// One accessor: it names something that exists, has already run, and is not
    /// inside a branch this reference is not part of.
    fn accessor_holds(
        &mut self,
        site: &str,
        from: Option<&str>,
        accessor: &Accessor,
        legs: &BTreeMap<String, Leg>,
        forms: &BTreeMap<String, BTreeSet<String>>,
        early_legs: &BTreeMap<String, BTreeSet<String>>,
    ) {
        match accessor {
            Accessor::Leg { leg, .. } => {
                if !legs.contains_key(leg) {
                    self.errors.push(PlanError::UnknownReference {
                        site: site.to_string(),
                        reference: leg.clone(),
                    });
                }
            }
            Accessor::Early { early, .. } => match early_legs.get(early) {
                None => self.errors.push(PlanError::UnknownEarlyDialog {
                    site: site.to_string(),
                    early: early.clone(),
                }),
                Some(owners) if owners.len() > 1 => {
                    self.errors.push(PlanError::AmbiguousEarlyDialog {
                        site: site.to_string(),
                        early: early.clone(),
                    })
                }
                Some(_) => {}
            },
            Accessor::Number { name, form } => {
                match forms.get(name) {
                    None => self.errors.push(PlanError::UnknownIdentity {
                        site: site.to_string(),
                        name: name.clone(),
                    }),
                    Some(declared) if !declared.contains(form) => {
                        self.errors.push(PlanError::UndeclaredDialForm {
                            site: site.to_string(),
                            name: name.clone(),
                            form: form.clone(),
                        })
                    }
                    Some(_) => {}
                }
            }
            Accessor::Step { step, field } => {
                if *field == StepField::Branch {
                    let is_alt = self
                        .program
                        .item_for(step)
                        .is_some_and(|i| i.kind == ItemKind::Alt && i.id == *step);
                    if !is_alt {
                        self.errors.push(PlanError::BranchAccessorOnNonAlt {
                            site: site.to_string(),
                            step: step.clone(),
                        });
                        return;
                    }
                }
                let owner = from.and_then(|id| self.steps.get(id)).cloned();
                self.reference_holds(site, step, owner.as_ref());
            }
        }
    }
}

/// Whether a request runs INSIDE an early dialog rather than waiting for a
/// confirmed one: PRACK (RFC 3262 §7.2) and UPDATE (RFC 3311 §5.1), and nothing
/// else. Both may name the fork they ride.
fn rides_early_dialog(step: &CompiledStep) -> bool {
    step.msg
        .method
        .as_deref()
        .map(sip_message::Method::from_wire)
        .is_some_and(|m| m == sip_message::Method::Prack || m == sip_message::Method::Update)
}

/// Whether the spec's frozen headers state `name` at all.
fn states_header(msg: &MsgSpec, name: &str) -> bool {
    msg.headers.iter().any(|h| h.name.eq_ignore_ascii_case(name))
}

/// Whether the spec's frozen headers require RFC 3262 reliability. Option-tag
/// reading is `sip-message`'s: this never splits a header value itself.
fn requires_100rel(msg: &MsgSpec) -> bool {
    use sip_message::header::HeaderValue;
    msg.headers
        .iter()
        .filter(|h| sip_message::HeaderName::Require.matches(&h.name))
        .filter_map(|h| sip_message::header::Require::parse(&sip_message::SipStr::owned(&h.value)).ok())
        .any(|tokens| tokens.contains("100rel"))
}

/// Every string a body spec carries — refs, modes, content types.
fn body_strings(body: &Body) -> Vec<String> {
    match body {
        Body::Resource(r) => {
            let mut out = vec![r.reference.clone()];
            out.extend(r.rewrite.iter().cloned());
            out.extend(r.content_type.iter().cloned());
            out
        }
        Body::Shape(_) => Vec::new(),
        Body::Multipart(m) => {
            let mut out = vec![m.multipart.content_type.clone()];
            for p in &m.multipart.parts {
                out.push(p.content_type.clone());
                out.push(p.reference.clone());
                out.extend(p.rewrite.iter().cloned());
                out.extend(p.content_id.iter().cloned());
                for header in &p.headers {
                    out.push(header.name.clone());
                    out.push(header.value.clone());
                }
                out.extend(p.cid_linked.iter().cloned());
            }
            out
        }
    }
}

/// A check's `op`/`value` agreement and, for `regex`, that the pattern compiles
/// (§9). Shared by the inline and postcondition sites.
pub fn check_shape(site: &str, check: &Check) -> Result<(), PlanError> {
    match (check.op, &check.value) {
        (CheckOp::Eq | CheckOp::Regex, None) => Err(PlanError::CheckValueMismatch {
            site: site.to_string(),
            op: format!("{:?}", check.op).to_lowercase(),
            detail: "requires a value".into(),
        }),
        (CheckOp::Exists | CheckOp::Absent, Some(_)) => Err(PlanError::CheckValueMismatch {
            site: site.to_string(),
            op: format!("{:?}", check.op).to_lowercase(),
            detail: "takes no value".into(),
        }),
        (CheckOp::Regex, Some(pattern)) if !pattern.contains("${") => {
            regex::Regex::new(pattern).map(|_| ()).map_err(|e| PlanError::CheckRegexInvalid {
                site: site.to_string(),
                pattern: pattern.clone(),
                detail: e.to_string(),
            })
        }
        _ => Ok(()),
    }
}
