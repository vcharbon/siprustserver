//! The rule engine — declarative match descriptors, a first-match/layer-ranked
//! executor, the action vocabulary + executor, framework invariants, and the
//! basic-B2BUA default rule set. Port of `src/b2bua/rules/`.

pub mod actions;
pub mod defaults;
pub mod docgen;
pub mod executor;
pub mod invariants;
pub mod model;
pub mod promote_pem;
pub mod refer_transfer;
pub mod relay;
pub mod relay_first_18x;
pub mod sdp_answer;
pub mod sdp_diff;
pub mod service;

pub use actions::ActionExecutor;
pub use defaults::default_rules;
pub use promote_pem::promote_pem_rules;
pub use refer_transfer::{transfer_rules, transfer_seed_rules};
pub use relay_first_18x::relay_first_18x_rules;
pub use executor::{execute_rules, pick_ranked};
pub use docgen::{check_registry, render_registry, MachineGraph};
pub use service::{compose_rules, seed_services, ServiceDef, ServiceSeed};
pub use model::{
    Match, MatchKind, MessageTransform, RuleAction, RuleContext, RuleDefinition, RuleHandleResult,
    StatusMatch, CORE_LAYER, SERVICE_LAYER,
};

// Re-exported for the `define_service!` / `sm_rule!` macros (and the public Rule
// SDK, slice 6) so authored services reference framework types through `$crate`.
pub use call::{Call, MachineId, StateLabel};
