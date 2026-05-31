//! The rule engine — declarative match descriptors, a first-match/layer-ranked
//! executor, the action vocabulary + executor, framework invariants, and the
//! basic-B2BUA default rule set. Port of `src/b2bua/rules/`.

pub mod actions;
pub mod defaults;
pub mod executor;
pub mod invariants;
pub mod model;
pub mod relay;

pub use actions::ActionExecutor;
pub use defaults::default_rules;
pub use executor::{execute_rules, pick_ranked};
pub use model::{
    Match, MatchKind, MessageTransform, RuleAction, RuleContext, RuleDefinition, RuleHandleResult,
    StatusMatch, CORE_LAYER, SERVICE_LAYER,
};
