//! The Callflow shape's **functional body** (ADR-0018): the compiled-Rust
//! message-sequence template that drives an [`InfraRuntime`], parameterised over
//! [`Input`]. The *same* body runs over any [`InfraShape`](crate::infra::InfraShape).
//!
//! What a shape *is* — its stable id, published anchor vocabulary, required
//! input and typed params schema — lives on its ONE declaration, the
//! [`e2e_model::ShapeDescriptor`] in the unified shape registry; this crate
//! ATTACHES the `!Send` body to that descriptor by id
//! (see [`crate::shapes::attach`]). The trait keeps only what the RUN needs:
//! the agent roster the body drives and whether it exchanges media.

use async_trait::async_trait;

use crate::infra::InfraRuntime;

pub use e2e_model::shape::Anchor;
/// The shared input CORE a Test case supplies to a shape (moved to
/// `e2e-model` as `CoreInput`; the historical `shape::Input` name is kept).
pub use e2e_model::shape::CoreInput as Input;

/// Whether a Callflow shape exchanges RTP audio alongside the signaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode {
    /// Signaling only.
    Off,
    /// Each Test Agent opens a `MediaEndpoint` on the same `SignalingNetwork`,
    /// streams its deterministic reference clip, and records inbound audio;
    /// per-agent `.wav` artifacts + the classifier verdict land on the result.
    Exchange,
}

/// A compiled Callflow shape's FUNCTIONAL BODY — attached to its
/// [`e2e_model::ShapeDescriptor`] by id in the registry (`crate::shapes`),
/// selected (not authored) by Test cases.
#[async_trait(?Send)]
pub trait CallflowShape {
    /// The Test-agent roster this body drives — the names a check's
    /// `<agent>.<anchor>` selector may bind to (e.g. `["alice", "bob1"]`). The
    /// `<agent>.<anchor>` autocomplete surface (ADR-0019) is the cartesian
    /// product `agents() × descriptor.anchors`. Defaults empty for bodies that
    /// have not declared a roster yet (no selector completion, but never a
    /// wrong one).
    ///
    /// NOTE (experimental): this is an explicit list mirroring the names the
    /// body passes to `rt.anchor(..)` in `run`; the no-drift design folds it
    /// into a per-shape `Agent` enum `run` is forced to use. Keep this in step
    /// with `run` until that lands.
    fn agents(&self) -> &[&str] {
        &[]
    }
    /// Whether this shape exchanges media (off by default).
    fn media(&self) -> MediaMode {
        MediaMode::Off
    }
    /// Drive the flow over the given Infra runtime. Receives the full authored
    /// [`Input`](crate::model::Input) — the shared `core` (From/To/R-URI) plus the
    /// shape's typed `extras` (read via [`Input::params`](crate::model::Input::params),
    /// schema'd by the descriptor's `params_schema`). Assertion failures panic
    /// in-line (the harness philosophy); the run-core isolates the panic per
    /// cell.
    async fn run(&self, rt: &mut InfraRuntime, input: &crate::model::Input);
}
