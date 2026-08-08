//! The rule engine — declarative match descriptors ([`model`]), a
//! first-match/layer-ranked executor, the action vocabulary + executor
//! ([`actions`]), framework invariants, and the rule families: the CORE
//! lifecycle set ([`defaults`]), the 18x-policy services ([`relay_first_18x`],
//! [`promote_pem`]), REFER transfer ([`refer_transfer`]), and release/reroute
//! ([`release_reroute`]).

pub mod actions;
pub mod capabilities;
pub mod defaults;
pub mod docgen;
pub mod executor;
pub mod invariants;
pub mod model;
pub mod promote_pem;
pub mod refer_transfer;
pub mod relay;
pub mod relay_first_18x;
pub mod release_reroute;
pub mod service;

pub use actions::ActionExecutor;
pub use defaults::{default_rules, default_rules_with, ComposeOptions};
pub use promote_pem::promote_pem_rules;
pub use refer_transfer::{transfer_rules, transfer_seed_rules, transfer_service_def};
pub use relay_first_18x::{relay_first_18x_rules, relay_first_18x_service_def};
pub use executor::{execute_rules, pick_ranked};
pub use docgen::{check_registry, render_registry, render_registry_html, MachineGraph};
pub use service::{compose_rules, seed_services, ServiceDef, ServiceSeed, Terminal};
pub use model::{
    Effect, EffectKind, Match, MatchKind, MessageTransform, RuleAction, RuleCall, RuleContext,
    RuleDefinition, RuleHandleResult, StatusMatch, CORE_LAYER, SERVICE_LAYER,
};

// Re-exported for the `define_service!` / `sm_rule!` macros (and the public Rule
// SDK, slice 6) so authored services reference framework types through `$crate`.
pub use call::{Call, MachineId, StateLabel};
