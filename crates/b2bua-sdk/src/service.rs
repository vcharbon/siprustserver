//! Callflow-service authoring surface (ADR-0016) — the declarative macros
//! ([`define_service!`] / [`sm_rule!`]) and the registry types ([`ServiceDef`] /
//! [`ServiceSeed`]).
//!
//! A **service** is a per-call state machine: a [`MachineId`](call::MachineId)
//! (== the service id), a set of state-gated rules ([`sm_rule!`]), and an `init`
//! hook (X8) run once at call setup that may seed the machine's initial cursor,
//! install its data backing, and fire an initial action batch. Everything a
//! service does rides the normal [`RuleAction`](crate::model::RuleAction)/effects
//! pipeline — there is no privileged back-door that writes call state outside
//! the executor. The engine-side composition glue (`compose_rules` /
//! `seed_services`, which need the executor) lives in `b2bua`, not here.

use call::StateLabel;
// `Call` is nameable here ONLY for the [`ServiceSeed::data_write`] installer
// (ADR-0016 X8's sanctioned one-shot data-backing write — a typed slice needs
// the full struct). Rule handlers and `init` read through the narrow
// [`RuleCall`](crate::model::RuleCall) view (ADR-0020 X8) and never see it.
pub use call::Call;

use crate::model::{RuleAction, RuleCall, RuleDefinition};

/// The terminal-state marker for a `sm_rule!` transition (ADR-0016 X9). Writing
/// `transitions: [ State::Bridging => Terminal ]` declares that the rule
/// **deactivates** the machine from `Bridging` (its handler emits
/// [`RuleAction::ClearState`](crate::model::RuleAction::ClearState), removing the
/// cursor). It carries the [`StateLabel::terminal`] sentinel, rendered as
/// Mermaid's `[*]` sink. A unit value with a `const label()` so it drops straight
/// into the macro's transition column beside the state-enum variants.
#[derive(Clone, Copy, Debug)]
pub struct Terminal;

impl Terminal {
    pub const fn label(self) -> StateLabel {
        StateLabel::terminal()
    }
}

/// What a service's `init` returns to seed its machine at call setup (ADR-0016
/// X8). All three parts are folded through the normal executor/effects pipeline
/// by the engine's `seed_services`:
/// - `initial_state` — the cursor written to `sm_cursors[service_id]`;
/// - `data_write` — a one-shot mutation installing the service's data backing
///   (a typed slice in-tree, or `ext[id]` for an out-of-crate integrator);
/// - `actions` — an initial [`RuleAction`] batch (e.g. the announcement service's
///   `CreateLeg{kind:media}` toward the MRF, launched in parallel with routing).
pub struct ServiceSeed {
    pub initial_state: StateLabel,
    pub data_write: Box<dyn FnOnce(&mut Call)>,
    pub actions: Vec<RuleAction>,
}

impl ServiceSeed {
    /// A seed that only sets the initial cursor (no data backing, no actions).
    pub fn new(initial_state: StateLabel) -> Self {
        Self {
            initial_state,
            data_write: Box::new(|_| {}),
            actions: Vec::new(),
        }
    }
    /// Install the service's data backing (typed slice or `ext[id]`).
    pub fn with_data(mut self, f: impl FnOnce(&mut Call) + 'static) -> Self {
        self.data_write = Box::new(f);
        self
    }
    /// Fire an initial action batch through the executor at setup.
    pub fn with_actions(mut self, actions: Vec<RuleAction>) -> Self {
        self.actions = actions;
        self
    }
}

/// A registered callflow service: its id (== its [`MachineId`](call::MachineId)),
/// the setup `init` hook (returns `None` to stay dormant — a vanilla call pays
/// nothing), and a factory for its state-gated rules.
pub struct ServiceDef {
    pub id: &'static str,
    pub init: fn(&RuleCall) -> Option<ServiceSeed>,
    pub rules: fn() -> Vec<RuleDefinition>,
}

/// Declare a callflow service (ADR-0016 X5): its machine id, its state enum, the
/// `init` hook, and its state-gated rules. Expands to — in the invoking module —
/// a `MachineId` const, the `Copy` state enum with a `const fn label()`, and the
/// `rules()` / `init()` / `service_def()` functions the registry composes.
///
/// ```ignore
/// define_service! {
///     id: "stub",
///     machine: STUB_MACHINE,
///     states: StubState { S0, S1 },
///     init: |call: &RuleCall| { /* -> Option<ServiceSeed> */ },
///     rules: [ advance_rule() ],
/// }
/// ```
#[macro_export]
macro_rules! define_service {
    (
        id: $id:literal,
        machine: $machine:ident,
        states: $state_enum:ident { $($variant:ident),+ $(,)? },
        init: $init:expr,
        rules: [ $($rule:expr),* $(,)? ] $(,)?
    ) => {
        /// Machine id for this service (== service id).
        pub const $machine: $crate::rules::MachineId = $crate::rules::MachineId::new($id);

        /// The service's declared states (ADR-0016). The compiler rejects any
        /// rule that references a non-existent variant.
        #[derive(::core::clone::Clone, ::core::marker::Copy, ::core::fmt::Debug,
                 ::core::cmp::PartialEq, ::core::cmp::Eq)]
        pub enum $state_enum { $($variant),+ }

        impl $state_enum {
            /// The wire label for this state (variant name).
            pub const fn label(self) -> $crate::rules::StateLabel {
                $crate::rules::StateLabel::new(match self {
                    $(Self::$variant => ::core::stringify!($variant)),+
                })
            }
        }

        /// This service's state-gated rules.
        pub fn rules() -> ::std::vec::Vec<$crate::rules::RuleDefinition> {
            ::std::vec![ $($rule),* ]
        }

        /// The setup `init` hook (X8): seed the machine, or `None` to stay dormant.
        /// Reads the narrow [`RuleCall`]($crate::rules::RuleCall) view (ADR-0020 X8).
        pub fn init(call: &$crate::rules::RuleCall) -> ::core::option::Option<$crate::rules::ServiceSeed> {
            let f: fn(&$crate::rules::RuleCall) -> ::core::option::Option<$crate::rules::ServiceSeed> = $init;
            f(call)
        }

        /// The registry descriptor composed into the engine.
        pub fn service_def() -> $crate::rules::ServiceDef {
            $crate::rules::ServiceDef { id: $id, init, rules }
        }
    };
}

/// Declare one state-gated rule of a service (ADR-0016 X5). `active` and
/// `transitions` take state-enum values (e.g. `StubState::S0`); the macro lifts
/// them into the `&'static [StateLabel]` declaration columns the engine and the
/// doc generator read. The `transitions` list is the `(from, to)` edges the
/// handle may cause via `SetState` (checked by the executor).
///
/// ```ignore
/// sm_rule! {
///     id: "stub-advance",
///     machine: STUB_MACHINE,
///     active: [ StubState::S0 ],
///     transitions: [ StubState::S0 => StubState::S1 ],
///     matcher: Match::request().method("INFO"),
///     handle: |_ctx| { /* -> Option<RuleHandleResult> */ },
/// }
/// ```
#[macro_export]
macro_rules! sm_rule {
    (
        id: $id:literal,
        machine: $machine:expr,
        active: [ $($act:expr),+ $(,)? ],
        transitions: [ $($from:expr => $to:expr),* $(,)? ],
        effects: [ $($effect:expr),* $(,)? ],
        matcher: $matcher:expr,
        handle: $handle:expr $(,)?
    ) => {{
        // `const` items (not inline `&[..]`) so the slices are `'static` despite
        // `StateLabel` / `Effect` carrying a `Cow` / `Method(String)` (which
        // `needs_drop`, blocking implicit promotion in a fn body).
        const __ACTIVE: &[$crate::rules::StateLabel] = &[ $($act.label()),+ ];
        const __TRANS: &[($crate::rules::StateLabel, $crate::rules::StateLabel)] =
            &[ $(($from.label(), $to.label())),* ];
        const __EFFECTS: &[$crate::rules::Effect] = &[ $($effect),* ];
        $crate::rules::RuleDefinition {
            id: $id,
            layer: $crate::rules::SERVICE_LAYER,
            overrides: &[],
            matcher: $matcher,
            handle: $handle,
            machine: ::core::option::Option::Some($machine),
            active_states: __ACTIVE,
            transitions: __TRANS,
            effects: __EFFECTS,
        }
    }};
}
