//! [`RuleDefinition`]: a rule as the engine ranks and selects it.

use call::{MachineId, StateLabel};

use crate::effect::Effect;
use crate::matcher::Match;
use crate::rule_context::RuleContext;
use crate::rule_handle_result::RuleHandleResult;

/// A rule: a named, layer-ranked event handler that returns actions or declines.
///
/// A rule may belong to a per-call **machine** (ADR-0016 X1): it is then a
/// selection candidate only when that machine's cursor is in [`Self::active_states`]
/// (see `executor::pick_ranked`). Machine-less ("core") rules — built via
/// [`Self::core`] — leave `machine`/`active_states`/`transitions` unset and are
/// always candidates. The `transitions` it declares are the `(from, to)` edges
/// its handle may cause via `SetState` (checked + diagram-generated in later
/// slices).
#[derive(Clone)]
pub struct RuleDefinition {
    pub id: &'static str,
    pub layer: u8,
    pub overrides: &'static [&'static str],
    pub matcher: Match,
    pub handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    /// Owner machine, or `None` for a machine-less core rule.
    pub machine: Option<MachineId>,
    /// States (within `machine`) in which this rule is a candidate.
    pub active_states: &'static [StateLabel],
    /// Declared `(from, to)` transition edges this rule's handle may cause.
    pub transitions: &'static [(StateLabel, StateLabel)],
    /// The **tracked side effects** this rule's handle may emit (ADR-0016 X9).
    /// Required for a machine-bound (`sm_rule!`) rule; empty for a core rule. The
    /// executor verifies the handler's emitted actions are a subset of these *by
    /// [`EffectKind`](crate::effect::EffectKind)* (cursor moves + bookkeeping are auto-allowed).
    pub effects: &'static [Effect],
    /// A teardown rule stays a selection candidate on a `Terminating` /
    /// `Terminated` call for an asynchronous trigger — a timer fire, a
    /// transaction timeout, an internal-event fold
    /// ([`CallEvent::is_asynchronous_trigger`](crate::event::CallEvent::is_asynchronous_trigger)). Every other rule is absorbed
    /// there: a call already going away makes no forward progress on its own
    /// clock. A peer's message and a CANCEL keep their per-rule filters.
    pub teardown: bool,
}

impl RuleDefinition {
    /// A machine-less ("core") rule: always a selection candidate, regardless of
    /// any machine cursor. Every core rule is built through this; only
    /// `sm_rule!`-generated service rules populate the machine fields directly.
    pub fn core(
        id: &'static str,
        layer: u8,
        overrides: &'static [&'static str],
        matcher: Match,
        handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    ) -> Self {
        Self {
            id,
            layer,
            overrides,
            matcher,
            handle,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }
    }

    /// Mark this a teardown rule ([`Self::teardown`]): it still runs on a
    /// going-away call for an asynchronous trigger — to resolve a leg, scrub
    /// a spent ledger entry, or reap a wedged teardown.
    pub fn runs_while_terminating(mut self) -> Self {
        self.teardown = true;
        self
    }
}
