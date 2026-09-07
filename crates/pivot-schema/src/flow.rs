//! The `flow` (`PCAP2TEST_PIVOT_V3.md` §6): the choreography, in order.
//!
//! A captured flow lists EVERY captured message, protocol-mechanical automatics
//! included, so a reader can tell "elided automatic" from "never happened". A
//! captured retransmission does not become a step: it collapses onto the step
//! it repeats, as a count.
//!
//! Four node kinds, discriminated by `op`: a message step (`send` / `expect`),
//! an injected external event (`inject`), a set of declared alternatives
//! (`alt`) and an order-free group (`unordered`). Nesting stops there — an
//! `alt` branch and an `unordered` group hold message steps, never further
//! blocks — because the interpreter commits on a branch's FIRST message and a
//! nested block has no first message to commit on.
//!
//! Same-leg ordering is list order; cross-leg ordering is `after`, which names
//! the steps that must have completed first. Ordering is message-mediated:
//! there are no state predicates.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::check::Check;
use crate::msg::MsgSpec;

/// One node of the flow.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FlowNode {
    /// One message at one actor's vantage. Boxed: it is by far the largest
    /// node, and every other variant would otherwise pay its size.
    Message(Box<Step>),
    /// An external event handed to the lane's injector.
    Inject(Inject),
    /// Declared alternatives: exactly one branch runs.
    Alt(Alt),
    /// Messages that must all arrive, in any order.
    Unordered(Unordered),
}

impl FlowNode {
    /// The node's own id.
    pub fn id(&self) -> &str {
        match self {
            FlowNode::Message(s) => &s.id,
            FlowNode::Inject(i) => &i.id,
            FlowNode::Alt(a) => &a.id,
            FlowNode::Unordered(u) => &u.id,
        }
    }

    /// The steps this node orders after.
    pub fn after(&self) -> &[String] {
        match self {
            FlowNode::Message(s) => &s.after,
            FlowNode::Inject(i) => &i.after,
            FlowNode::Alt(a) => &a.after,
            FlowNode::Unordered(u) => &u.after,
        }
    }

    /// Every message step the node contains, in document order.
    pub fn steps(&self) -> Vec<&Step> {
        match self {
            FlowNode::Message(step) => vec![step.as_ref()],
            FlowNode::Inject(_) => Vec::new(),
            FlowNode::Alt(alt) => alt.branches.iter().flat_map(|b| b.steps.iter()).collect(),
            FlowNode::Unordered(group) => group.steps.iter().collect(),
        }
    }
}

/// Dispatch on `op` by hand rather than through an untagged enum: an untagged
/// mismatch reports "matched no variant", and the field a generator misspelled
/// is exactly what its author needs told.
///
/// The node is held as its RAW JSON text and parsed twice — once to read `op`,
/// once into the variant it names. Buffering through `serde_json::Value`
/// instead would silently collapse a duplicate key to the last one, so
/// `{"op":"expect","op":"send"}` would parse; both passes here stream through
/// derived impls, which refuse a duplicate field at any depth exactly as every
/// other object in this crate does. JSON is the pivot's only serialization, and
/// a format that cannot hand back raw text fails loudly rather than quietly.
impl<'de> Deserialize<'de> for FlowNode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        let text = raw.get();
        let probe: OpProbe = serde_json::from_str(text).map_err(D::Error::custom)?;
        let op = probe.op.ok_or_else(|| D::Error::custom("a flow node states its `op`"))?;
        let node = match op.as_str() {
            "send" | "expect" => {
                serde_json::from_str(text).map(|step| FlowNode::Message(Box::new(step)))
            }
            "inject" => serde_json::from_str(text).map(FlowNode::Inject),
            "alt" => serde_json::from_str(text).map(FlowNode::Alt),
            "unordered" => serde_json::from_str(text).map(FlowNode::Unordered),
            other => {
                return Err(D::Error::custom(format!(
                    "flow node op {other:?} is none of send, expect, inject, alt, unordered"
                )));
            }
        };
        node.map_err(D::Error::custom)
    }
}

/// Reads `op` and ignores the rest. The derived impl still refuses a duplicate
/// `op`, which is the point of probing rather than indexing a parsed map.
#[derive(Deserialize)]
struct OpProbe {
    op: Option<String>,
}

impl JsonSchema for FlowNode {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("FlowNode")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let step = generator.subschema_for::<Step>();
        let inject = generator.subschema_for::<Inject>();
        let alt = generator.subschema_for::<Alt>();
        let unordered = generator.subschema_for::<Unordered>();
        json_schema!({
            "description": "One flow node, discriminated by `op`: a message step, an `inject`, an `alt` or an `unordered` group.",
            "oneOf": [step, inject, alt, unordered],
        })
    }
}

/// One message at one actor's vantage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Unique within the document. Tool-generated for a capture (`s7`),
    /// author-chosen otherwise. Every reference in the document is to this id;
    /// nothing references a position.
    pub id: String,
    /// The leg this message rides.
    pub leg: String,
    /// Whether the actor emits the message or waits for it.
    pub op: Op,
    /// `true` when the interpreter's own stack COMPOSES this message (100
    /// Trying, ACK-to-final, PRACK and its 2xx), deriving its R-URI, Route, Via
    /// and CSeq from the transaction that obliged it rather than from dialog
    /// state (§6.3). It is not a storage policy: the step stores what any step
    /// stores, and only a body the stack could not place is withheld.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto: bool,
    /// `true` when the message belongs to a transaction inside an ESTABLISHED
    /// dialog. Generic, method-blind and TOTAL (§6.1): every step of a leg
    /// after that leg's dialog-creating final carries it, none at or before it
    /// does, and a CANCEL never does. What it marks on a final is that the final
    /// answers a re-negotiation rather than creating the dialog, and an
    /// in-dialog final never closes a leg (§4.1), so a `cause` may not cite one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_dialog: bool,
    /// `true` on the one ACK that answers a leg's dialog-creating final, and on
    /// no other step (§6.1). It separates the ACK completing the dialog
    /// handshake from an ordinary in-dialog ACK — a re-INVITE's — which the
    /// method cannot and a CSeq chase may not. It rides BESIDE `in_dialog`,
    /// never instead of it, and beside `early` where the ACK names its fork.
    #[serde(default, skip_serializing_if = "is_false")]
    pub confirms_dialog: bool,
    /// Captured retransmissions of THIS message, beyond the first. Appears on
    /// `send` and `expect` alike: either side of a vantage can retransmit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmits: Option<u32>,
    /// The gap, in ms, before each repeat this step declares — one entry per
    /// repeat, measured from the emission before it. A CAPTURED ladder's own
    /// pacing: `retransmits` says how many rungs and this says when, so a peer
    /// whose ladder is not the RFC's is replayed as it ran. Empty — on every
    /// authored step, and on a generated one whose producer measured none — the
    /// ladder falls back to the RFC pacing for the message class (§6.9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retransmit_intervals_ms: Vec<u64>,
    /// `expect` only: whether the stored content is matched or merely recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckMode>,
    /// `expect` only: tolerated absence. The step is released when a later step
    /// on the same leg matches first.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    /// Ids of steps that must have completed before this one runs. The one
    /// cross-leg and cross-call ordering device.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Field assertions over the matched message, in the one check vocabulary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    /// Id of the early dialog the step rides: one a UAS-simulated fork answers
    /// under (a response `send` carries the id), or one the run observes the
    /// peer forking (a response `expect` carries it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub early: Option<String>,
    /// A declared race: this step and the named one may arrive in either order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<String>,
    /// The message itself, under the three-tier model (§8).
    pub msg: MsgSpec,
    /// When the step runs, relative to what.
    pub delay: Delay,
    /// Per-step override of `timing.expect_budget_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
    /// Capture coordinate. Captured documents only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<Observed>,
}

/// An external event the lane's injector performs. The interpreter NEVER
/// executes an action: it hands the token to an injector interface the
/// deployment provides, which is why one op spans store faults, HTTP-fabric
/// faults and node kills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inject {
    /// Unique within the document; what `after` and a delay anchor name.
    pub id: String,
    /// Always `inject`.
    pub op: InjectOp,
    /// Open action token from the deployment's injector registry
    /// (`store-fault:LiveAudit`, `http:bl-cut`, `node-kill`).
    pub action: String,
    /// What the action applies to, in the injector's own vocabulary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Ids of steps that must have completed before the action is handed over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Dwell before the action, anchored like a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<Delay>,
}

/// Declared alternatives. The interpreter commits on the first discriminating
/// message and NEVER backtracks, so branches must differ at their first
/// message — which lint enforces structurally rather than trusting an author.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Alt {
    /// Unique within the document. Referenced from outside the alt in place of
    /// any step inside it, and by `${step:<alt-id>.branch}`.
    pub id: String,
    /// Always `alt`.
    pub op: AltOp,
    /// The alternatives, two or more, discriminable at their first message.
    pub branches: Vec<Branch>,
    /// Ids of steps that must have completed before the alt opens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// One alternative. Its `name` is what a later assertion cites
/// (`${step:<alt-id>.branch}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Branch {
    /// Unique within the alt; what `${step:<alt-id>.branch}` resolves to.
    pub name: String,
    /// The branch's messages, in order. Message steps only — a nested block
    /// would have no first message to commit on.
    pub steps: Vec<Step>,
}

/// Messages that must ALL arrive, in any order — the declarative form of a
/// race whose outcome does not matter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Unordered {
    /// Unique within the document; what a later reference names.
    pub id: String,
    /// Always `unordered`.
    pub op: UnorderedOp,
    /// The messages, all of which must arrive. They share one place in the
    /// ordering, so none may reference another.
    pub steps: Vec<Step>,
    /// Ids of steps that must have completed before the group opens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// `auto` and `optional` are omitted when false: no field's emptiness carries
/// meaning (§2.2).
fn is_false(flag: &bool) -> bool {
    !*flag
}

/// Whether the actor emits the message or waits for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// The actor emits the message.
    Send,
    /// The actor waits for the message, within its budget.
    Expect,
}

/// The `op` of an `inject` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum InjectOp {
    Inject,
}

/// The `op` of an `alt` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AltOp {
    Alt,
}

/// The `op` of an `unordered` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UnorderedOp {
    Unordered,
}

/// What an `expect` does with the content it stored. The GENERATOR decides and
/// the document says which; a lane never picks the meaning. Under both values
/// the step still gates on `op`, leg alignment, the discriminator and
/// `within_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckMode {
    /// The stored content is MATCHED against the inbound message. Applies where
    /// some captured peer emitted the message and the system relayed it.
    Assert,
    /// The stored content is RECORDED for post-run confrontation and matched
    /// against nothing. Applies where the system MINTED the message.
    Record,
}

/// Dwell from an explicit anchor. There are no absolute times in a pivot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Delay {
    /// Milliseconds from the anchor.
    pub ms: u64,
    /// What the dwell is measured from: the case's start, or a named step.
    pub from: Anchor,
    /// Whether a virtual-clock lane may compress this dwell. STATED, never
    /// re-derived: a lane that compresses reads this and nothing else.
    pub compressible: bool,
    /// Whether the dwell interacts with a system or session timer.
    pub timer_linked: bool,
}

/// What a dwell is measured from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// The moment the case starts.
    Trigger,
    /// The named step's own message.
    Step(String),
}

impl Anchor {
    /// The step id this anchor names, if it names one.
    pub fn step(&self) -> Option<&str> {
        match self {
            Anchor::Trigger => None,
            Anchor::Step(id) => Some(id),
        }
    }
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Anchor::Trigger => f.write_str("trigger"),
            Anchor::Step(id) => write!(f, "step:{id}"),
        }
    }
}

impl FromStr for Anchor {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "trigger" {
            return Ok(Anchor::Trigger);
        }
        match s.strip_prefix("step:") {
            Some("") => Err("delay anchor 'step:' names no step".into()),
            Some(id) => Ok(Anchor::Step(id.to_string())),
            None => Err(format!("delay anchor {s:?} is neither 'trigger' nor 'step:<id>'")),
        }
    }
}

crate::string_token!(
    Anchor,
    "Delay anchor: `trigger` or `step:<id>`.",
    "^(trigger|step:[^ ]+)$"
);

/// The step's coordinate in the flows document. Informative: the interpreter
/// never reads it. It exists because the post-run confrontation has to pair a
/// run step with the captured message it is compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observed {
    /// Leg index in the flows document.
    pub leg: usize,
    /// Message index within that leg.
    pub msg: usize,
    /// Microseconds from the case's FIRST captured message.
    pub at_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anchor_round_trips_through_its_token() {
        for token in ["trigger", "step:s1", "step:invite-out"] {
            assert_eq!(token.parse::<Anchor>().unwrap().to_string(), token);
        }
    }

    #[test]
    fn a_malformed_anchor_is_refused() {
        for token in ["step:", "start", "", "s1"] {
            assert!(token.parse::<Anchor>().is_err(), "{token:?} should be refused");
        }
    }

    fn node(text: &str) -> FlowNode {
        serde_json::from_str(text).unwrap()
    }

    const DELAY: &str = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;

    #[test]
    fn each_op_decodes_to_its_own_node_kind() {
        let step = format!(
            r#"{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{DELAY}}}"#
        );
        assert!(matches!(node(&step), FlowNode::Message(_)));
        assert!(matches!(
            node(r#"{"id":"i1","op":"inject","action":"node-kill","target":"b"}"#),
            FlowNode::Inject(_)
        ));
        assert!(matches!(
            node(r#"{"id":"a1","op":"alt","branches":[{"name":"x","steps":[]}]}"#),
            FlowNode::Alt(_)
        ));
        assert!(matches!(
            node(r#"{"id":"u1","op":"unordered","steps":[]}"#),
            FlowNode::Unordered(_)
        ));
    }

    /// The reason [`FlowNode`] dispatches by hand: an untagged enum would
    /// report "matched no variant" and hide the field that is actually wrong.
    #[test]
    fn a_misspelled_field_names_itself_in_the_error() {
        let text = format!(
            r#"{{"id":"s1","leg":"A","op":"send","mgs":{{"method":"INVITE"}},"delay":{DELAY}}}"#
        );
        let error = serde_json::from_str::<FlowNode>(&text).unwrap_err().to_string();
        assert!(error.contains("mgs"), "{error}");
    }

    #[test]
    fn an_unknown_op_is_refused_by_name() {
        let error = serde_json::from_str::<FlowNode>(r#"{"id":"x","op":"snd"}"#).unwrap_err().to_string();
        assert!(error.contains("snd"), "{error}");
        assert!(serde_json::from_str::<FlowNode>(r#"{"id":"x"}"#).is_err());
    }

    #[test]
    fn flags_are_omitted_when_false_so_emptiness_never_carries_meaning() {
        let text = format!(
            r#"{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{DELAY}}}"#
        );
        let FlowNode::Message(step) = node(&text) else { panic!("a message step") };
        assert!(!step.auto && !step.optional);
        let json = serde_json::to_string(&step).unwrap();
        assert!(!json.contains("auto") && !json.contains("optional"), "{json}");
    }

    #[test]
    fn a_block_reports_the_steps_it_contains() {
        let alt = node(
            r#"{"id":"a1","op":"alt","branches":[
                 {"name":"cancelled","steps":[{"id":"s2","leg":"A","op":"expect","msg":{"status":487},
                   "delay":{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}}]},
                 {"name":"answered","steps":[{"id":"s3","leg":"A","op":"expect","msg":{"status":200},
                   "delay":{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}}]}]}"#,
        );
        assert_eq!(alt.steps().iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["s2", "s3"]);
        assert_eq!(alt.id(), "a1");
    }
}
