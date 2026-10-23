//! The **unified, OPEN shape registry** — ONE vocabulary of Callflow shapes,
//! each declared exactly once as a [`ShapeDescriptor`], shared by BOTH run
//! surfaces:
//!
//!   - the **load** surface (`crates/loadgen`) consumes the descriptor's
//!     load-side attributes (the named callee [`LegSpec`]s — or their
//!     `needs_charlie` / `needs_bob2` sugar — plus `emergency` / mix weights)
//!     and its optional [`ActorScenario`] body factory;
//!   - the **functional** surface (`crates/e2e-core`) ATTACHES its `!Send`
//!     `CallflowShape` bodies to the same descriptors **by id** (the body
//!     drives an `InfraRuntime`, which cannot live in this dependency-light
//!     crate), and validates authored Test cases against the descriptor's
//!     [`ShapeSpec`] metadata (anchors / required input).
//!
//! A shape may carry one body, the other, or **both** (a *dual-body* shape —
//! `rerouting_prack` is the first): the descriptor is the single declaration of
//! its id, anchor vocabulary and load attributes, so the two surfaces can never
//! drift apart on what a shape *is*.
//!
//! The registry is a plain **builder**: [`ShapeRegistry::with_defaults`] loads
//! every shipped shape in one call; a third-party crate starts from that (or
//! [`ShapeRegistry::empty`]) and [`register`](ShapeRegistry::register)s its own
//! descriptors. One id space — a duplicate id panics at registration, never a
//! silent shadow.

use std::collections::BTreeMap;
use std::sync::Arc;

// The shipped load bodies are COMPOSED through the callshapes pipeline algebra
// — same ids, same downstream contract as the
// hand-written `scenario_harness::actor::scenarios` bodies.
use callshapes::shapes as cs;
use scenario_harness::actor::ActorScenario;

use crate::shape::{Anchor, ShapeSpec};

mod source;
pub use source::{intern, intern_legs, intern_prefixes, ShapeSource};

/// Per-run **scenario inputs** — SUT auth data (and any future non-topology
/// per-run value) fed into load-body *construction*. Deliberately NOT part of
/// the per-call `CallEnv`: the env carries the environment axis (agents,
/// egress seam, timing), while these are what the e2e model calls a Test
/// case's input/extras.
#[derive(Clone, Debug)]
pub struct ScenarioInputs {
    /// The `X-Api-Call.refer_key` the SUT's REFER backend authorizes (the CLI's
    /// `--refer-key`); consumed by the refer scenarios.
    pub refer_key: String,
}

impl Default for ScenarioInputs {
    fn default() -> Self {
        // The scripted `/call/refer` backend's canonical allow key (the same
        // default as the CLI's `--refer-key`).
        Self { refer_key: "refer-allow-c".to_string() }
    }
}

/// A shape's minted **load body**: an ACTOR-declared scenario — per-endpoint
/// reactive actors the runner joins (`scenario_harness::actor::run_actor_scenario`).
/// The load driver's `run_one` drives it; everything downstream (teardown,
/// classification, sampling) is shared. `.id()` resolves through
/// [`ActorScenario`].
pub type Scenario = Arc<dyn ActorScenario>;

/// Factory minting a shape's **load body** from the per-run [`ScenarioInputs`]
/// (the refer scenarios take the run's `refer_key` at construction; stateless
/// bodies ignore the inputs).
pub type LoadFactory = Arc<dyn Fn(&ScenarioInputs) -> Scenario + Send + Sync>;

/// One **named callee leg** of a load shape: which receiver the load driver
/// binds on the shared UAS socket and which R-URI user-part prefixes select it
/// there. Generalizes the closed `needs_bob2`/`needs_charlie` label list — an
/// open-registry shape's legs arrive under dialled R-URI digits, never under
/// the agent name, so label and prefix must be declared independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LegSpec {
    /// Agent name the load body binds/choreographs against (`"bob"`, `"mrf"`,
    /// …) — the label the driver's leg picker returns and the `CallEnv` callee
    /// role key.
    pub role: &'static str,
    /// R-URI user-part prefixes that select this leg on the shared UAS socket.
    /// Longest match wins across ALL legs' prefixes (a transfer target
    /// `9001100100555000111` must beat a sibling `9001100100` — the same rule
    /// `callee_group` applies), and several prefixes may select one leg (a
    /// callee reachable under more than one number form).
    pub ruri_prefixes: &'static [&'static str],
}

/// The boolean sugar's expansion targets: the default callee labels,
/// role == single prefix == label, so a shape without `legs` is byte-identical
/// on the wire.
const LEG_BOB: LegSpec = LegSpec { role: "bob", ruri_prefixes: &["bob"] };
const LEG_BOB2: LegSpec = LegSpec { role: "bob2", ruri_prefixes: &["bob2"] };
const LEG_CHARLIE: LegSpec = LegSpec { role: "charlie", ruri_prefixes: &["charlie"] };

impl LegSpec {
    /// The default `[bob(, bob2)(, charlie)]` expansion of the
    /// `needs_bob2`/`needs_charlie` sugar, in the load-bearing bind order.
    pub const fn historic(needs_bob2: bool, needs_charlie: bool) -> &'static [LegSpec] {
        match (needs_bob2, needs_charlie) {
            (false, false) => &[LEG_BOB],
            (true, false) => &[LEG_BOB, LEG_BOB2],
            (false, true) => &[LEG_BOB, LEG_CHARLIE],
            (true, true) => &[LEG_BOB, LEG_BOB2, LEG_CHARLIE],
        }
    }
}

/// The ONE declaration of a Callflow shape (see the module docs): its stable
/// **id** (the Test-case/campaign selector, the load report/metrics label and
/// the functional-body attachment key — one id space), its load-time metadata
/// ([`ShapeSpec`]: published anchors + required input), its optional authoring
/// **params schema** (typed `input.extras`), the **load-side attributes** the
/// driver consults per call, and the optional **load body** factory. The
/// functional body is attached by `e2e-core` (it cannot live here).
#[derive(Clone)]
pub struct ShapeDescriptor {
    /// Stable shape id — one id space across both run surfaces.
    pub id: &'static str,
    /// The canonical anchors this shape publishes (per-agent at runtime).
    /// BOTH bodies publish: a functional body tags via `InfraRuntime::anchor`
    /// (agents `alice`/`bob1`/`bob2`); a load body via `CallCtx::anchor` in the
    /// shared choreography (agents `alice`/`bob`/`bob2`/`charlie`, tagged only
    /// on a sampled call).
    pub anchors: &'static [Anchor],
    /// Input core/extra field names this shape *requires* (beyond the always-
    /// optional core overrides).
    pub required_input: &'static [&'static str],
    /// JSON schema for the shape's typed authoring parameters (spliced over the
    /// Test case's open `extras` map by the editor projection). `None` = no
    /// declared params.
    pub params_schema: Option<serde_json::Value>,
    /// Load: this shape's **named callee legs** ([`LegSpec`]: agent role +
    /// R-URI user-part prefixes) on the shared UAS socket, in the load-bearing
    /// bind order. Empty (the default) = derive from the
    /// `needs_bob2`/`needs_charlie` sugar — consume via
    /// [`callee_legs`](Self::callee_legs), never this field directly.
    pub legs: &'static [LegSpec],
    /// Load: this shape needs a third (transfer-target) callee leg bound.
    /// Pure sugar over [`legs`](Self::legs) (ignored when `legs` is declared).
    pub needs_charlie: bool,
    /// Load: this shape needs a SECOND callee receiver (`bob2`) sharing the
    /// callee socket — the rerouting shapes' failover target, demuxed by the
    /// driver's R-URI-user leg picker. Pure sugar over [`legs`](Self::legs)
    /// (ignored when `legs` is declared).
    pub needs_bob2: bool,
    /// Load: this call is an emergency (`Resource-Priority: esnet.0`) — the SUT
    /// force-admits it under overload, so it must never be shed.
    pub emergency: bool,
    /// Load: weight in the shipped DEFAULT mix (`None` = not in the default mix).
    pub default_weight: Option<f64>,
    /// Load: weight in the shipped ABNORMAL-END mix — the post-call cleanup
    /// coverage set, one shape per teardown path a call takes when it does not
    /// connect and hang up (each succeeds under its own accepted branch);
    /// `None` = not in that mix.
    pub abnormal_end_weight: Option<f64>,
    /// The optional load body factory (`None` = functional-only shape).
    pub load: Option<LoadFactory>,
}

impl std::fmt::Debug for ShapeDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShapeDescriptor")
            .field("id", &self.id)
            .field("anchors", &self.anchors)
            .field("required_input", &self.required_input)
            .field("legs", &self.legs)
            .field("needs_charlie", &self.needs_charlie)
            .field("needs_bob2", &self.needs_bob2)
            .field("emergency", &self.emergency)
            .field("default_weight", &self.default_weight)
            .field("abnormal_end_weight", &self.abnormal_end_weight)
            .field("has_load_body", &self.load.is_some())
            .finish()
    }
}

impl ShapeDescriptor {
    /// A new descriptor with every attribute at its default (no anchors, no
    /// requirements, no load attributes, no bodies). Chain the builder methods.
    pub fn new(id: &'static str) -> Self {
        ShapeDescriptor {
            id,
            anchors: &[],
            required_input: &[],
            params_schema: None,
            legs: &[],
            needs_charlie: false,
            needs_bob2: false,
            emergency: false,
            default_weight: None,
            abnormal_end_weight: None,
            load: None,
        }
    }

    pub fn anchors(mut self, anchors: &'static [Anchor]) -> Self {
        self.anchors = anchors;
        self
    }

    pub fn required_input(mut self, fields: &'static [&'static str]) -> Self {
        self.required_input = fields;
        self
    }

    pub fn params_schema(mut self, schema: serde_json::Value) -> Self {
        self.params_schema = Some(schema);
        self
    }

    /// Declare the shape's named callee legs (role + R-URI user-part prefixes,
    /// in bind order) — the open form the boolean sugar expands to. Overrides
    /// `needs_bob2`/`needs_charlie`.
    pub fn legs(mut self, legs: &'static [LegSpec]) -> Self {
        self.legs = legs;
        self
    }

    /// The effective callee-leg declaration the load driver binds and demuxes
    /// from: [`legs`](Self::legs) verbatim when declared, else the default
    /// `[bob(, bob2)(, charlie)]` expansion of the boolean sugar (role ==
    /// single prefix == label).
    pub fn callee_legs(&self) -> &'static [LegSpec] {
        if !self.legs.is_empty() {
            return self.legs;
        }
        LegSpec::historic(self.needs_bob2, self.needs_charlie)
    }

    pub fn needs_charlie(mut self) -> Self {
        self.needs_charlie = true;
        self
    }

    pub fn needs_bob2(mut self) -> Self {
        self.needs_bob2 = true;
        self
    }

    pub fn emergency(mut self) -> Self {
        self.emergency = true;
        self
    }

    pub fn default_weight(mut self, weight: f64) -> Self {
        self.default_weight = Some(weight);
        self
    }

    pub fn abnormal_end_weight(mut self, weight: f64) -> Self {
        self.abnormal_end_weight = Some(weight);
        self
    }

    /// Attach an ACTOR load body built fresh from the per-run [`ScenarioInputs`]
    /// (the refer scenarios take the run's `refer_key` at construction;
    /// stateless bodies ignore the inputs).
    pub fn load_actor_with(
        mut self,
        factory: impl Fn(&ScenarioInputs) -> Arc<dyn ActorScenario> + Send + Sync + 'static,
    ) -> Self {
        self.load = Some(Arc::new(move |i: &ScenarioInputs| factory(i) as Scenario));
        self
    }

    /// Attach a stateless, inputs-independent ACTOR load body (the common case).
    pub fn load_shared(self, body: Arc<dyn ActorScenario>) -> Self {
        self.load_actor_with(move |_| body.clone())
    }

    /// Mint this shape's load body from the per-run inputs (`None` =
    /// functional-only shape).
    pub fn load_scenario(&self, inputs: &ScenarioInputs) -> Option<Scenario> {
        self.load.as_ref().map(|f| f(inputs))
    }
}

impl ShapeSpec for ShapeDescriptor {
    fn anchors(&self) -> &[Anchor] {
        self.anchors
    }
    fn required_input(&self) -> &[&str] {
        self.required_input
    }
}

/// The open registry: one [`ShapeDescriptor`] per id, ordered (BTreeMap) so
/// every derived listing is deterministic.
#[derive(Default)]
pub struct ShapeRegistry {
    shapes: BTreeMap<String, ShapeDescriptor>,
}

impl ShapeRegistry {
    /// An empty registry (a third-party catalog built from scratch).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Every shipped shape in one call — the defaults both run surfaces start
    /// from. Extend with [`register`](Self::register).
    pub fn with_defaults() -> Self {
        let mut reg = Self::empty();
        for shape in default_shapes() {
            reg.register(shape);
        }
        reg
    }

    /// Register a shape. One id space: a duplicate id panics loudly at
    /// registration (a third-party shape may not shadow a shipped one).
    pub fn register(&mut self, shape: ShapeDescriptor) -> &mut Self {
        self.try_register(shape).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn get(&self, id: &str) -> Option<&ShapeDescriptor> {
        self.shapes.get(id)
    }

    /// Every registered id, sorted.
    pub fn ids(&self) -> Vec<String> {
        self.shapes.keys().cloned().collect()
    }

    /// Iterate the descriptors in id order.
    pub fn iter(&self) -> impl Iterator<Item = &ShapeDescriptor> {
        self.shapes.values()
    }

    /// Mint the load body for `id` from the per-run inputs. `None` when the id
    /// is unknown OR the shape has no load body (functional-only).
    pub fn load_scenario(&self, id: &str, inputs: &ScenarioInputs) -> Option<Scenario> {
        self.get(id)?.load_scenario(inputs)
    }

    /// The shapes of the shipped DEFAULT load mix (those with a
    /// `default_weight`), in id order.
    pub fn default_mix(&self) -> Vec<&ShapeDescriptor> {
        self.iter().filter(|d| d.default_weight.is_some()).collect()
    }

    /// The ABNORMAL-END load shapes (one per post-call-cleanup teardown path),
    /// in id order.
    pub fn abnormal_end_mix(&self) -> Vec<&ShapeDescriptor> {
        self.iter().filter(|d| d.abnormal_end_weight.is_some()).collect()
    }
}

// ---------------------------------------------------------------------------
// Shipped shape declarations — THE one place each shape's vocabulary lives.
// ---------------------------------------------------------------------------

/// Authoring parameters for the `rerouting` shape — the typed `input.extras`
/// the editor suggests. All optional (defaults reproduce the canonical 486
/// flow). Declared here —
/// next to the shape's descriptor — and consumed by the functional body in
/// `e2e-core` via `Input::params`.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ReroutingParams {
    /// SIP status bob1 rejects the first b-leg with, triggering the SUT's
    /// failover to bob2. Any 4xx–6xx the decision engine treats as a reroute
    /// trigger (default `486` Busy Here; e.g. `503` Service Unavailable).
    pub reject_status: u16,
    /// Reason phrase paired with `rejectStatus` (default `"Busy Here"`).
    pub reject_reason: String,
}

impl Default for ReroutingParams {
    fn default() -> Self {
        ReroutingParams { reject_status: 486, reject_reason: "Busy Here".to_string() }
    }
}

/// The anchor set of a plain full-call shape.
const CALL_ANCHORS: &[Anchor] = &[Anchor::InitialInvite, Anchor::Answer, Anchor::Ack, Anchor::Bye];
/// The anchor set the standard LOAD establishment + teardown publishes (the
/// actor caller's INVITE + BYE feeds): the plain set plus the 180 (tagged only
/// when it arrived — a non-PRACK provisional is best-effort, so key it from an
/// `optional` block).
const LOAD_CALL_ANCHORS: &[Anchor] =
    &[Anchor::InitialInvite, Anchor::FirstProvisional, Anchor::Answer, Anchor::Ack, Anchor::Bye];
/// The load `reinvite` shape: the shared establishment plus the re-INVITE.
const LOAD_REINVITE_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::ReInvite,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];
/// The load `refer` shape: hand-rolled establishment + the REFER (a SENT
/// anchor on bob — its only receiver is the SUT) + charlie's transfer INVITE
/// (`charlie.initialInvite`). Its teardown is scenario-owned (no bye anchor).
const LOAD_REFER_ANCHORS: &[Anchor] =
    &[Anchor::InitialInvite, Anchor::FirstProvisional, Anchor::Answer, Anchor::Ack, Anchor::Refer];
/// The load `long_call` shape: the shared establishment only — its tolerant
/// teardown absorbs bob's BYE in a quiesce (no bye anchor).
const LOAD_ESTABLISH_ANCHORS: &[Anchor] =
    &[Anchor::InitialInvite, Anchor::FirstProvisional, Anchor::Answer, Anchor::Ack];
/// The `rerouting_prack` anchor set: rerouting + the reliable-provisional dance.
const PRACK_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::Prack,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];
/// The `transfer-refer-media` anchor set.
const TRANSFER_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Refer,
    Anchor::ReInvite,
    Anchor::Bye,
];

/// Every shipped shape, declared once. Grouped by surface:
///
///  - **load-only** — the fallible real-call scenarios (`basic_call`,
///    `reinvite`, …);
///  - **functional-only** — the anchored e2e shapes (`basic-call`, `rerouting`,
///    …), whose `!Send` bodies `e2e-core` attaches by id;
///  - **dual-body** — `rerouting_prack`: ONE descriptor, both bodies.
fn default_shapes() -> Vec<ShapeDescriptor> {
    let mut shapes = vec![
        // ── Load: the happy-path real-call scenarios ─────────────────────────
        // ACTOR-executed: same downstream contract
        // (table §5.1).
        ShapeDescriptor::new("basic_call")
            .anchors(LOAD_CALL_ANCHORS)
            .default_weight(4.0)
            .load_actor_with(|_| Arc::new(cs::basic_call(cs::default_binder()))),
        // ACTOR-executed: same downstream contract
        // (table §5.2).
        ShapeDescriptor::new("reinvite")
            .anchors(LOAD_REINVITE_ANCHORS)
            .default_weight(2.0)
            .load_actor_with(|_| Arc::new(cs::reinvite(cs::default_binder()))),
        // C6: N serialized re-INVITE cycles. Small default weight (a ×10 call
        // carries ~10× the datagrams — keep it a light share of the mix).
        ShapeDescriptor::new("reinvite10")
            .anchors(LOAD_REINVITE_ANCHORS)
            .default_weight(0.5)
            .load_actor_with(|_| Arc::new(cs::reinvite_n(cs::default_binder(), "reinvite10", 10))),
        // C3/S3: crossing BYE (both ends hang up at once).
        ShapeDescriptor::new("crossing_bye")
            .anchors(LOAD_CALL_ANCHORS)
            .default_weight(1.0)
            .load_actor_with(|_| Arc::new(cs::crossing_bye(cs::default_binder()))),
        // C1/E3: TRUE forking — bob emits distinct-tag 18x on one INVITE txn.
        // Only valid under the SUT's transparent CORE relay.
        // NOTE: `forked_loser_late_200` is intentionally NOT registered — a
        // terminating B2BUA absorbs the loser's late 2xx, so it can't be
        // exercised through a SUT (see the shape's doc; SUT-less machinery
        // tests cover it).
        ShapeDescriptor::new("forked")
            .anchors(LOAD_CALL_ANCHORS)
            .default_weight(1.0)
            .load_actor_with(|_| Arc::new(cs::forked(cs::default_binder()))),
        ShapeDescriptor::new("forked_reliable")
            .anchors(PRACK_ANCHORS)
            .default_weight(0.5)
            .load_actor_with(|_| Arc::new(cs::forked_reliable(cs::default_binder()))),
        // A forked establishment answered by a fork that never rang (a fresh
        // To-tag on the 2xx, RFC 3261 §13.2.2.4). Id-addressable.
        ShapeDescriptor::new("forked_unrung")
            .anchors(LOAD_CALL_ANCHORS)
            .load_actor_with(|_| Arc::new(cs::forked_unrung(cs::default_binder()))),
        // C2/E5: CANCEL×200 crossing — a branch-aware race (Ok OR abandoned
        // timeout, both legal). A light share of the mix.
        ShapeDescriptor::new("cancel_answer_crossing")
            .anchors(LOAD_ESTABLISH_ANCHORS)
            .default_weight(0.5)
            .load_actor_with(|_| Arc::new(cs::cancel_answer_crossing(cs::default_binder()))),
        // C5: early UPDATE on the reliable early dialog (RFC 3311 §5.1).
        ShapeDescriptor::new("prack_update_early")
            .anchors(PRACK_ANCHORS)
            .default_weight(0.5)
            .load_actor_with(|_| Arc::new(cs::prack_update_early(cs::default_binder()))),
        // ACTOR-executed: per-endpoint reactive actors + the ack-gated settle barrier;
        // same downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.3).
        ShapeDescriptor::new("refer")
            .anchors(LOAD_REFER_ANCHORS)
            .default_weight(1.0)
            .needs_charlie()
            .load_actor_with(|inputs| Arc::new(cs::refer(cs::default_binder(), &inputs.refer_key))),
        // ACTOR-executed: same downstream contract
        // (table §5.4).
        ShapeDescriptor::new("options_hold")
            .anchors(LOAD_CALL_ANCHORS)
            .default_weight(1.0)
            .load_actor_with(|_| Arc::new(cs::options_hold(cs::default_binder()))),
        // ACTOR-executed: the reactors answer SUT
        // keepalives on both legs during the hold (table §5.5).
        ShapeDescriptor::new("long_call")
            .anchors(LOAD_ESTABLISH_ANCHORS)
            .load_actor_with(|_| Arc::new(cs::long_call(cs::default_binder()))),
        // ACTOR-executed: per-endpoint reactors + the
        // ack-gated settle barrier; same downstream contract (table §5.6).
        ShapeDescriptor::new("prack_update")
            .anchors(PRACK_ANCHORS)
            .load_actor_with(|_| Arc::new(cs::prack_update(cs::default_binder()))),
        // ── Load: the emergency variants (same flows, force-admitted) ────────
        ShapeDescriptor::new("basic_call_em")
            .anchors(LOAD_CALL_ANCHORS)
            .emergency()
            .load_actor_with(|_| Arc::new(cs::basic_call(cs::default_binder()))),
        ShapeDescriptor::new("reinvite_em")
            .anchors(LOAD_REINVITE_ANCHORS)
            .emergency()
            .load_actor_with(|_| Arc::new(cs::reinvite(cs::default_binder()))),
        // ── Load: the abnormal-end cleanup-coverage set ──────────────────────
        // ACTOR-executed: same downstream contract
        // (table §5.8).
        ShapeDescriptor::new("invite_reject")
            .abnormal_end_weight(1.0)
            .load_actor_with(|_| Arc::new(cs::invite_reject(cs::default_binder()))),
        // ACTOR-executed: same downstream contract
        // (table §5.9).
        ShapeDescriptor::new("abandon_ringing")
            .abnormal_end_weight(1.0)
            .load_actor_with(|_| Arc::new(cs::abandon_ringing(cs::default_binder()))),
        // ACTOR-executed: per-endpoint reactors + the
        // ack-gated settle barrier; same downstream contract (table §5.10).
        ShapeDescriptor::new("refer_charlie_reject")
            .abnormal_end_weight(1.0)
            .needs_charlie()
            .load_actor_with(|inputs| {
                Arc::new(cs::refer_charlie_reject(cs::default_binder(), &inputs.refer_key))
            }),
        // ── Functional: the anchored e2e shapes ──────────────────────────────
        ShapeDescriptor::new("basic-call").anchors(CALL_ANCHORS),
        ShapeDescriptor::new("basic-call-media").anchors(CALL_ANCHORS),
        ShapeDescriptor::new("rerouting").anchors(CALL_ANCHORS).params_schema(
            serde_json::to_value(schemars::schema_for!(ReroutingParams))
                .expect("ReroutingParams schema"),
        ),
        ShapeDescriptor::new("transfer-refer-media").anchors(TRANSFER_ANCHORS),
        // ── DUAL-BODY: rerouting + reliable provisional on the winning leg ───
        // One descriptor, one id: `e2e-core` attaches the functional (anchored,
        // panic-on-deviation) body; the load body below drives the same flow
        // fallibly through the egress candidate list ([bob, bob2] → the
        // `X-Api-Call` routes failover plan on a pinned layout).
        // The load body is ACTOR-executed (establishes the
        // reactive 100rel/PRACK machinery); the functional body stays in
        // `e2e-core`. Same downstream contract (table §5.7).
        ShapeDescriptor::new("rerouting_prack")
            .anchors(PRACK_ANCHORS)
            .needs_bob2()
            .load_actor_with(|_| Arc::new(cs::rerouting_prack(cs::default_binder()))),
        // NO-ANSWER-triggered failover — bob rings then never answers, the
        // SUT's per-route no-answer timer CANCELs it (487) and fails over to
        // bob2, which answers plainly. Id-addressable (no default mix weight),
        // like the matrix reroute cells.
        ShapeDescriptor::new("rerouting_noanswer")
            .anchors(LOAD_CALL_ANCHORS)
            .needs_bob2()
            .load_actor_with(|_| Arc::new(cs::rerouting_noanswer(cs::default_binder()))),
    ];
    // The DECLARED-compatibility-matrix cross-product cells (`crate::matrix`) —
    // id-addressable, no mix weight (the default mix samples the canonical +
    // catalog shapes; the full matrix is addressable by id for
    // targeted runs and the loss soaks).
    shapes.extend(crate::matrix::generated_shapes());
    shapes
}

#[cfg(test)]
mod tests;
