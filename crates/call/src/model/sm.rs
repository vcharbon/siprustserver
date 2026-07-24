//! State-machine identifiers (ADR-0016): [`MachineId`] names a per-call
//! machine, [`StateLabel`] a state within one. The cursor map itself lives on
//! [`Call::sm_cursors`](crate::model::Call::sm_cursors); rendering it is
//! [`crate::helpers::dump_cursors`].

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// Identifier of a per-call state machine (ADR-0016 X4). Newtype over a
/// `Cow<'static, str>` so a `RuleDefinition` can declare its machine with a
/// compile-time `&'static str` literal (`MachineId::new`), while a replicated
/// `Call` body deserialises the same type into an owned string. Used both as a
/// static rule-declaration column and as the
/// [`Call::sm_cursors`](crate::model::Call::sm_cursors) map key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MachineId(pub Cow<'static, str>);

impl MachineId {
    /// Construct from a compile-time literal (usable in `const`/`static`).
    pub const fn new(s: &'static str) -> Self {
        MachineId(Cow::Borrowed(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A state label within a machine (ADR-0016 X4). Newtype over a
/// `Cow<'static, str>` for the same reason as [`MachineId`]: static rule
/// declarations carry `&'static str` literals; replicated cursors deserialise
/// into owned strings.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StateLabel(pub Cow<'static, str>);

impl StateLabel {
    /// Construct from a compile-time literal (usable in `const`/`static`).
    pub const fn new(s: &'static str) -> Self {
        StateLabel(Cow::Borrowed(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The **terminal** sentinel (ADR-0016 X9 machine deactivation) — a declared
    /// transition `S => terminal` means a rule deactivates the machine from `S`
    /// (removing its cursor). Rendered as Mermaid's `[*]` sink; never a real
    /// cursor value (a deactivated machine has *no* cursor).
    pub const fn terminal() -> Self {
        StateLabel(Cow::Borrowed("[*]"))
    }
    /// Is this the [`terminal`](Self::terminal) sentinel?
    pub fn is_terminal(&self) -> bool {
        self.0 == "[*]"
    }
}
