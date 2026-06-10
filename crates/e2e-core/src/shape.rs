//! Callflow shapes (ADR-0018) + the canonical Message-anchor vocabulary
//! (ADR-0019). A shape is a compiled-Rust message-sequence template parameterised
//! over [`Input`], publishing the anchors it produces. The *same* shape runs over
//! any [`InfraShape`](crate::infra::InfraShape).

use async_trait::async_trait;

use crate::infra::InfraRuntime;

/// The canonical, project-wide Message-anchor vocabulary. A Callflow shape
/// publishes the subset it produces; a Check binds to `<agent>.<anchor>`
/// (ADR-0019). Extend deliberately — adding a common anchor is a project-wide act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Anchor {
    InitialInvite,
    ReInvite,
    FirstProvisional,
    Answer,
    Ack,
    Bye,
    Refer,
    Prack,
}

impl Anchor {
    /// The canonical surface name used in authored JSON (`bob1.initialInvite`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Anchor::InitialInvite => "initialInvite",
            Anchor::ReInvite => "reInvite",
            Anchor::FirstProvisional => "firstProvisional",
            Anchor::Answer => "answer",
            Anchor::Ack => "ack",
            Anchor::Bye => "bye",
            Anchor::Refer => "refer",
            Anchor::Prack => "prack",
        }
    }

    pub const ALL: &'static [Anchor] = &[
        Anchor::InitialInvite,
        Anchor::ReInvite,
        Anchor::FirstProvisional,
        Anchor::Answer,
        Anchor::Ack,
        Anchor::Bye,
        Anchor::Refer,
        Anchor::Prack,
    ];

    /// Parse a surface name back into the vocabulary (`None` = not a canonical
    /// anchor — a load-time validation error, never a silent pass).
    pub fn parse(name: &str) -> Option<Anchor> {
        Anchor::ALL.iter().copied().find(|a| a.as_str() == name)
    }
}

/// The shared input CORE a Test case supplies to a shape: From / To / R-URI
/// overrides (the numbers), each optional. This is both the runtime input and
/// the `core` of the authored JSON [`Input`](crate::model::Input) — headers /
/// timers join when the harness builder can honour them (no silent fields).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(rename = "CoreInput")]
pub struct Input {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruri: Option<String>,
}

/// A compiled Callflow shape — selected (not authored) from the registry.
#[async_trait(?Send)]
pub trait CallflowShape {
    /// Stable id used by Test cases / campaigns to select this shape.
    fn id(&self) -> &str;
    /// The anchors this shape publishes (per-agent at runtime).
    fn anchors(&self) -> &[Anchor];
    /// Input core/extra field names this shape *requires* (beyond the always-
    /// optional core overrides). A Test case missing one is incompatible at
    /// load time.
    fn required_input(&self) -> &[&str] {
        &[]
    }
    /// Drive the flow over the given Infra runtime. Assertion failures panic
    /// in-line (the harness philosophy); the run-core isolates the panic per cell.
    async fn run(&self, rt: &mut InfraRuntime, input: &Input);
}
