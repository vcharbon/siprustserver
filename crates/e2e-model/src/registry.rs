//! The **unified, OPEN shape registry** — ONE vocabulary of Callflow shapes,
//! each declared exactly once as a [`ShapeDescriptor`], shared by BOTH run
//! surfaces:
//!
//!   - the **load** surface (`crates/loadgen`) consumes the descriptor's
//!     load-side attributes (`needs_charlie` / `needs_bob2` / `emergency` /
//!     mix weights) and its optional [`RealCallScenario`] body factory;
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

use scenario_harness::realcall::scenarios::{
    AbandonRinging, BasicCall, InviteReject, LongCall, OptionsHold, PrackUpdate, Refer,
    ReferCharlieReject, Reinvite, ReroutingPrack,
};
use scenario_harness::realcall::RealCallScenario;

use crate::shape::{Anchor, ShapeSpec};

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

/// Factory minting a shape's **load body** from the per-run [`ScenarioInputs`]
/// (the refer scenarios take the run's `refer_key` at construction; stateless
/// bodies ignore the inputs).
pub type LoadFactory = Arc<dyn Fn(&ScenarioInputs) -> Arc<dyn RealCallScenario> + Send + Sync>;

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
    /// Load: this shape needs a third (transfer-target) callee leg bound.
    pub needs_charlie: bool,
    /// Load: this shape needs a SECOND callee receiver (`bob2`) sharing the
    /// callee socket — the rerouting shapes' failover target, demuxed by the
    /// driver's R-URI-user leg picker.
    pub needs_bob2: bool,
    /// Load: this call is an emergency (`Resource-Priority: esnet.0`) — the SUT
    /// force-admits it under overload, so it must never be shed.
    pub emergency: bool,
    /// Load: weight in the shipped DEFAULT mix (`None` = not in the default mix).
    pub default_weight: Option<f64>,
    /// Load: weight in the shipped voluntarily-FAILING mix (the post-call
    /// cleanup-coverage set); `None` = not a failure-mix shape.
    pub failure_weight: Option<f64>,
    /// The optional load body factory (`None` = functional-only shape).
    pub load: Option<LoadFactory>,
}

impl std::fmt::Debug for ShapeDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShapeDescriptor")
            .field("id", &self.id)
            .field("anchors", &self.anchors)
            .field("required_input", &self.required_input)
            .field("needs_charlie", &self.needs_charlie)
            .field("needs_bob2", &self.needs_bob2)
            .field("emergency", &self.emergency)
            .field("default_weight", &self.default_weight)
            .field("failure_weight", &self.failure_weight)
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
            needs_charlie: false,
            needs_bob2: false,
            emergency: false,
            default_weight: None,
            failure_weight: None,
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

    pub fn failure_weight(mut self, weight: f64) -> Self {
        self.failure_weight = Some(weight);
        self
    }

    /// Attach a load body built fresh from the per-run [`ScenarioInputs`].
    pub fn load_with(
        mut self,
        factory: impl Fn(&ScenarioInputs) -> Arc<dyn RealCallScenario> + Send + Sync + 'static,
    ) -> Self {
        self.load = Some(Arc::new(factory));
        self
    }

    /// Attach a stateless, inputs-independent load body (the common case).
    pub fn load_shared(self, body: Arc<dyn RealCallScenario>) -> Self {
        self.load_with(move |_| body.clone())
    }

    /// Mint this shape's load body from the per-run inputs (`None` =
    /// functional-only shape).
    pub fn load_scenario(&self, inputs: &ScenarioInputs) -> Option<Arc<dyn RealCallScenario>> {
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
        let prev = self.shapes.insert(shape.id.to_string(), shape);
        if let Some(prev) = prev {
            panic!("duplicate shape id {:?} in the registry (one id space)", prev.id);
        }
        self
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
    pub fn load_scenario(
        &self,
        id: &str,
        inputs: &ScenarioInputs,
    ) -> Option<Arc<dyn RealCallScenario>> {
        self.get(id)?.load_scenario(inputs)
    }

    /// The shapes of the shipped DEFAULT load mix (those with a
    /// `default_weight`), in id order.
    pub fn default_mix(&self) -> Vec<&ShapeDescriptor> {
        self.iter().filter(|d| d.default_weight.is_some()).collect()
    }

    /// The voluntarily-FAILING load shapes (one per post-call-cleanup teardown
    /// path), in id order.
    pub fn failure_mix(&self) -> Vec<&ShapeDescriptor> {
        self.iter().filter(|d| d.failure_weight.is_some()).collect()
    }
}

// ---------------------------------------------------------------------------
// Shipped shape declarations — THE one place each shape's vocabulary lives.
// ---------------------------------------------------------------------------

/// Authoring parameters for the `rerouting` shape — the typed `input.extras`
/// the editor suggests. All optional (defaults reproduce the canonical 486
/// flow), so a case that sets none behaves exactly as before. Declared here —
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
/// The anchor set the shared LOAD establishment publishes (`realcall::establish`
/// + `hangup`): the plain set plus the 180 (tagged only when it arrived — a
/// non-PRACK provisional is best-effort, so key it from an `optional` block).
const LOAD_CALL_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];
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
const LOAD_REFER_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Refer,
];
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
///  - **load-only** — the fallible real-call scenarios (report/metrics ids
///    unchanged: `basic_call`, `reinvite`, …);
///  - **functional-only** — the anchored e2e shapes (`basic-call`, `rerouting`,
///    …), whose `!Send` bodies `e2e-core` attaches by id;
///  - **dual-body** — `rerouting_prack`: ONE descriptor, both bodies.
fn default_shapes() -> Vec<ShapeDescriptor> {
    vec![
        // ── Load: the happy-path real-call scenarios ─────────────────────────
        ShapeDescriptor::new("basic_call")
            .anchors(LOAD_CALL_ANCHORS)
            .default_weight(4.0)
            .load_shared(Arc::new(BasicCall)),
        ShapeDescriptor::new("reinvite")
            .anchors(LOAD_REINVITE_ANCHORS)
            .default_weight(2.0)
            .load_shared(Arc::new(Reinvite)),
        ShapeDescriptor::new("refer")
            .anchors(LOAD_REFER_ANCHORS)
            .default_weight(1.0)
            .needs_charlie()
            .load_with(|inputs| Arc::new(Refer::new(&inputs.refer_key))),
        ShapeDescriptor::new("options_hold")
            .anchors(LOAD_CALL_ANCHORS)
            .default_weight(1.0)
            .load_shared(Arc::new(OptionsHold)),
        ShapeDescriptor::new("long_call")
            .anchors(LOAD_ESTABLISH_ANCHORS)
            .load_shared(Arc::new(LongCall)),
        ShapeDescriptor::new("prack_update")
            .anchors(PRACK_ANCHORS)
            .load_shared(Arc::new(PrackUpdate)),
        // ── Load: the emergency variants (same flows, force-admitted) ────────
        ShapeDescriptor::new("basic_call_em")
            .anchors(LOAD_CALL_ANCHORS)
            .emergency()
            .load_shared(Arc::new(BasicCall)),
        ShapeDescriptor::new("reinvite_em")
            .anchors(LOAD_REINVITE_ANCHORS)
            .emergency()
            .load_shared(Arc::new(Reinvite)),
        // ── Load: the voluntarily-failing cleanup-coverage set ───────────────
        ShapeDescriptor::new("invite_reject")
            .failure_weight(1.0)
            .load_shared(Arc::new(InviteReject)),
        ShapeDescriptor::new("abandon_ringing")
            .failure_weight(1.0)
            .load_shared(Arc::new(AbandonRinging)),
        ShapeDescriptor::new("refer_charlie_reject")
            .failure_weight(1.0)
            .needs_charlie()
            .load_with(|inputs| Arc::new(ReferCharlieReject::new(&inputs.refer_key))),
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
        ShapeDescriptor::new("rerouting_prack")
            .anchors(PRACK_ANCHORS)
            .needs_bob2()
            .load_shared(Arc::new(ReroutingPrack)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_register_one_id_space_with_both_surfaces() {
        let reg = ShapeRegistry::with_defaults();
        // Load ids unchanged (report/metrics compatibility).
        for id in [
            "basic_call",
            "reinvite",
            "refer",
            "options_hold",
            "long_call",
            "prack_update",
            "basic_call_em",
            "reinvite_em",
            "invite_reject",
            "abandon_ringing",
            "refer_charlie_reject",
        ] {
            let d = reg.get(id).unwrap_or_else(|| panic!("load shape {id:?} missing"));
            assert!(d.load.is_some(), "{id} carries a load body");
        }
        // Functional ids unchanged; no load body.
        for id in ["basic-call", "basic-call-media", "rerouting", "transfer-refer-media"] {
            let d = reg.get(id).unwrap_or_else(|| panic!("functional shape {id:?} missing"));
            assert!(d.load.is_none(), "{id} is functional-only");
            assert!(!ShapeSpec::anchors(d).is_empty(), "{id} publishes anchors");
        }
        // The dual-body shape carries BOTH the anchor vocabulary and a load body.
        let dual = reg.get("rerouting_prack").expect("dual-body shape registered");
        assert!(dual.load.is_some() && !dual.anchors.is_empty());
        assert!(dual.needs_bob2, "rerouting needs the second callee receiver");
    }

    #[test]
    fn default_and_failure_mixes_match_the_historic_tables() {
        let reg = ShapeRegistry::with_defaults();
        let mix: Vec<(&str, f64)> =
            reg.default_mix().iter().map(|d| (d.id, d.default_weight.unwrap())).collect();
        assert_eq!(
            mix,
            vec![("basic_call", 4.0), ("options_hold", 1.0), ("refer", 1.0), ("reinvite", 2.0)]
        );
        let failures: Vec<&str> = reg.failure_mix().iter().map(|d| d.id).collect();
        assert_eq!(failures, vec!["abandon_ringing", "invite_reject", "refer_charlie_reject"]);
    }

    #[test]
    fn load_scenario_constructs_from_inputs() {
        let reg = ShapeRegistry::with_defaults();
        let inputs = ScenarioInputs { refer_key: "k-42".into() };
        assert_eq!(reg.load_scenario("basic_call", &inputs).unwrap().id(), "basic_call");
        assert_eq!(reg.load_scenario("refer", &inputs).unwrap().id(), "refer");
        assert!(reg.load_scenario("basic-call", &inputs).is_none(), "functional-only: no body");
        assert!(reg.load_scenario("nope", &inputs).is_none());
    }

    #[test]
    fn third_party_registration_is_open() {
        let mut reg = ShapeRegistry::with_defaults();
        reg.register(
            ShapeDescriptor::new("vendor_flow")
                .anchors(CALL_ANCHORS)
                .load_shared(Arc::new(BasicCall)),
        );
        assert!(reg.get("vendor_flow").is_some());
        assert!(reg.ids().contains(&"vendor_flow".to_string()));
    }

    #[test]
    #[should_panic(expected = "duplicate shape id")]
    fn duplicate_id_panics() {
        let mut reg = ShapeRegistry::with_defaults();
        reg.register(ShapeDescriptor::new("basic_call"));
    }
}
