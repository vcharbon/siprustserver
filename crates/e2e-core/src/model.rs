//! The authored JSON surface (ADR-0018 Phase D): Test cases, Check sets and
//! Campaigns as `serde` + `schemars::JsonSchema` documents, plus the loader and
//! the **load-time compatibility validation** (ADR-0019): a Test case is
//! compatible with a Callflow shape iff (i) its input satisfies the shape's
//! `required_input`, and (ii) every `<agent>.<anchor>` its checks (inline or via
//! referenced Check sets) bind to names an anchor the shape publishes. An
//! incompatible case fails loudly at load, never silently at run.
//!
//! `xtask e2e-schema` emits one `e2e/schemas/<doc>.schema.json` per top-level
//! doc type here; authored files reference it via `$schema` for editor
//! completion. Per-shape input *extras* are an open `extras` map for now — the
//! schema `if/then` discriminator lands with the first shape that declares
//! extras (none does yet).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use schemars::{JsonSchema, Schema, schema_for};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::infra::EndpointConfig;
use crate::shape::{Anchor, CallflowShape, Input as CoreInput};

/// The input a Test case feeds a shape: the shared **core** (From / To / R-URI
/// overrides — always optional) plus per-shape **extras** (an open map until a
/// shape declares a typed extras schema).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Input {
    #[serde(default)]
    pub core: CoreInput,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, serde_json::Value>,
}

impl Input {
    /// Does this input provide the named field (a set core field or an extras
    /// key)? Drives the `required_input` half of compatibility.
    pub fn provides(&self, field: &str) -> bool {
        match field {
            "from" => self.core.from.is_some(),
            "to" => self.core.to.is_some(),
            "ruri" => self.core.ruri.is_some(),
            other => self.extras.contains_key(other),
        }
    }

    /// Deserialize the open `extras` map into a shape's typed parameters `T`
    /// (paired with its `JsonSchema` via [`CallflowShape::params_schema`], so the
    /// schema the author saw IS the type `run` reads). `T` should carry
    /// `#[serde(default)]` so an absent/partial `extras` yields its defaults; a
    /// *malformed* one panics loudly (the harness philosophy — never a silent
    /// fallback to defaults that masks a typo).
    pub fn params<T: DeserializeOwned>(&self) -> T {
        let obj = serde_json::Value::Object(self.extras.clone().into_iter().collect());
        serde_json::from_value(obj)
            .unwrap_or_else(|e| panic!("invalid shape params in `extras`: {e}"))
    }
}

/// The assertion operator of a [`Check`] (ADR-0019).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CheckOp {
    /// `value` is a regex the extracted field must match.
    Regex,
    /// `value` (after `${…}` substitution) must equal the extracted field.
    Eq,
    /// The field must be present (no `value`).
    Exists,
    /// The field must be absent (no `value`).
    Absent,
}

/// One field assertion over the anchored message. The field grammar
/// (ADR-0019): URI-bearing headers expose `.userInfo/.host/.port/.displayName/`
/// `.tag/.param(x)` (e.g. `from.userInfo`); any other header gets
/// present/absent/regex over its raw value (`header(X-Foo)`); the payload is
/// `body`; the transport endpoints are `source.ip/.port` / `dest.ip/.port`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Check {
    /// Field selector, e.g. `from.userInfo`, `header(P-Asserted-Identity)`,
    /// `body`, `source.ip`.
    pub field: String,
    pub op: CheckOp,
    /// Expected value: a literal, a regex (op `regex`), or a binding —
    /// `${input.from}`, `${infra.lbVip}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// A bundle of [`Check`]s bound to one `<agent>.<anchor>` message selector.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckBlock {
    /// `<agent>.<anchor>`, e.g. `bob1.initialInvite`. The anchor must come from
    /// the canonical vocabulary and be published by every compatible shape.
    pub on: String,
    /// An optional block whose selector matches no recorded message is skipped;
    /// a non-optional one **fails** (ADR-0019).
    #[serde(default)]
    pub optional: bool,
    pub checks: Vec<Check>,
}

impl CheckBlock {
    /// Split `on` into `(agent, anchor-name)`.
    pub fn selector(&self) -> Option<(&str, &str)> {
        self.on.split_once('.').filter(|(a, n)| !a.is_empty() && !n.is_empty())
    }
}

/// A committed, shareable bundle of check blocks (e.g. `invite-identity`),
/// reusable by every Test case whose shapes publish the referenced anchors.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckSet {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub blocks: Vec<CheckBlock>,
}

/// An authored Test case: input data + checks + the Callflow shapes it is
/// compatible with (validated, never assumed).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestCase {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub compatible_shapes: Vec<String>,
    #[serde(default)]
    pub input: Input,
    /// Ids of shared [`CheckSet`]s this case pulls in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check_sets: Vec<String>,
    /// Case-local check blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckBlock>,
}

/// Per-[`InfraKind`](crate::infra::InfraKind) run-executor concurrency caps:
/// fake cells fan out wide; real cells share one external cluster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Concurrency {
    #[serde(default = "default_fake_concurrency")]
    pub fake: usize,
    #[serde(default = "default_real_concurrency")]
    pub real: usize,
}

fn default_fake_concurrency() -> usize {
    8
}
fn default_real_concurrency() -> usize {
    1
}

impl Default for Concurrency {
    fn default() -> Self {
        Concurrency { fake: default_fake_concurrency(), real: default_real_concurrency() }
    }
}

/// A campaign: the {case × compatible shape × infra} matrix to expand and run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Campaign {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Test-case ids.
    pub cases: Vec<String>,
    /// Infra-shape ids to run each compatible cell over.
    pub infra_shapes: Vec<String>,
    #[serde(default)]
    pub concurrency: Concurrency,
}

/// Every authored doc type, as `(file stem, JSON schema)` — the source of
/// truth `xtask e2e-schema` writes to `e2e/schemas/<stem>.schema.json`.
pub fn schemas() -> Vec<(&'static str, Schema)> {
    vec![
        ("endpoint-config", schema_for!(EndpointConfig)),
        ("test-case", schema_for!(TestCase)),
        ("check-set", schema_for!(CheckSet)),
        ("campaign", schema_for!(Campaign)),
    ]
}

/// Phase-D model errors: file-level load failures and load-time validation
/// failures (every problem listed, with the case/shape/anchor named).
#[derive(Debug)]
pub enum ModelError {
    Io { path: PathBuf, source: std::io::Error },
    Parse { path: PathBuf, source: serde_json::Error },
    Invalid(Vec<String>),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::Io { path, source } => write!(f, "read {}: {source}", path.display()),
            ModelError::Parse { path, source } => write!(f, "parse {}: {source}", path.display()),
            ModelError::Invalid(problems) => {
                write!(f, "validation failed:")?;
                for p in problems {
                    write!(f, "\n  - {p}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ModelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ModelError::Io { source, .. } => Some(source),
            ModelError::Parse { source, .. } => Some(source),
            ModelError::Invalid(_) => None,
        }
    }
}

fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T, ModelError> {
    let text = std::fs::read_to_string(path)
        .map_err(|source| ModelError::Io { path: path.to_path_buf(), source })?;
    serde_json::from_str(&text)
        .map_err(|source| ModelError::Parse { path: path.to_path_buf(), source })
}

pub fn load_test_case(path: &Path) -> Result<TestCase, ModelError> {
    load_json(path)
}

pub fn load_check_set(path: &Path) -> Result<CheckSet, ModelError> {
    load_json(path)
}

pub fn load_campaign(path: &Path) -> Result<Campaign, ModelError> {
    load_json(path)
}

pub fn load_endpoint_config(path: &Path) -> Result<EndpointConfig, ModelError> {
    load_json(path)
}

/// Load every `*.json` Check set in a directory, keyed by its `id`. A missing
/// directory is an empty store (Check sets are optional); duplicate ids fail.
pub fn load_check_sets(dir: &Path) -> Result<BTreeMap<String, CheckSet>, ModelError> {
    let mut sets = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(sets),
        Err(source) => return Err(ModelError::Io { path: dir.to_path_buf(), source }),
    };
    for entry in entries {
        let path = entry
            .map_err(|source| ModelError::Io { path: dir.to_path_buf(), source })?
            .path();
        if path.extension().is_some_and(|e| e == "json") {
            let set = load_check_set(&path)?;
            if let Some(dup) = sets.insert(set.id.clone(), set) {
                return Err(ModelError::Invalid(vec![format!(
                    "duplicate check-set id {:?} (second definition in {})",
                    dup.id,
                    path.display()
                )]));
            }
        }
    }
    Ok(sets)
}

/// The load-time compatibility validation (ADR-0019). Checks, per compatible
/// shape: (i) the input provides every `required_input` field; (ii) every
/// anchor referenced by the case's checks — inline and via its Check sets — is
/// canonical and published by the shape. Plus shape/check-set resolution and
/// op/value coherence. Returns **all** problems, not just the first.
pub fn validate_case(
    case: &TestCase,
    shapes: &BTreeMap<String, Box<dyn CallflowShape>>,
    check_sets: &BTreeMap<String, CheckSet>,
) -> Result<(), ModelError> {
    let mut problems = Vec::new();
    let case_id = &case.id;

    if case.compatible_shapes.is_empty() {
        problems.push(format!("test case {case_id:?}: compatibleShapes is empty"));
    }

    // Resolve the referenced Check sets; collect every block the case binds.
    let mut blocks: Vec<&CheckBlock> = case.checks.iter().collect();
    for set_id in &case.check_sets {
        match check_sets.get(set_id) {
            Some(set) => blocks.extend(set.blocks.iter()),
            None => problems.push(format!(
                "test case {case_id:?}: unknown check set {set_id:?} (known: [{}])",
                keys(check_sets)
            )),
        }
    }

    // Shape-independent block lints: selector form, canonical anchor, op/value.
    for block in &blocks {
        match block.selector() {
            Some((_agent, anchor_name)) => {
                if Anchor::parse(anchor_name).is_none() {
                    problems.push(format!(
                        "test case {case_id:?}: check {:?} uses anchor {anchor_name:?} which is \
                         not in the canonical vocabulary [{}]",
                        block.on,
                        Anchor::ALL.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
            None => problems.push(format!(
                "test case {case_id:?}: check selector {:?} must be `<agent>.<anchor>`",
                block.on
            )),
        }
        for check in &block.checks {
            let needs_value = matches!(check.op, CheckOp::Regex | CheckOp::Eq);
            if needs_value && check.value.is_none() {
                problems.push(format!(
                    "test case {case_id:?}: check {:?} field {:?} op {:?} requires a value",
                    block.on, check.field, check.op
                ));
            }
            if !needs_value && check.value.is_some() {
                problems.push(format!(
                    "test case {case_id:?}: check {:?} field {:?} op {:?} takes no value",
                    block.on, check.field, check.op
                ));
            }
        }
    }

    // Per compatible shape: resolution, required input, published anchors.
    for shape_id in &case.compatible_shapes {
        let Some(shape) = shapes.get(shape_id) else {
            problems.push(format!(
                "test case {case_id:?}: unknown Callflow shape {shape_id:?} (known: [{}])",
                keys(shapes)
            ));
            continue;
        };
        for field in shape.required_input() {
            if !case.input.provides(field) {
                problems.push(format!(
                    "test case {case_id:?} is incompatible with shape {shape_id:?}: required \
                     input field {field:?} is missing"
                ));
            }
        }
        for block in &blocks {
            let Some((_agent, anchor_name)) = block.selector() else { continue };
            let Some(anchor) = Anchor::parse(anchor_name) else { continue };
            if !shape.anchors().contains(&anchor) {
                problems.push(format!(
                    "test case {case_id:?} is incompatible with shape {shape_id:?}: check {:?} \
                     references anchor {anchor_name:?} but the shape only publishes [{}]",
                    block.on,
                    shape.anchors().iter().map(|a| a.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    if problems.is_empty() { Ok(()) } else { Err(ModelError::Invalid(problems)) }
}

fn keys<V>(map: &BTreeMap<String, V>) -> String {
    map.keys().map(|k| format!("{k:?}")).collect::<Vec<_>>().join(", ")
}
