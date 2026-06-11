//! Callflow-service engine glue (ADR-0016) — the composition + init-seeding
//! helpers that wire a service's declared rules and `init` seeds through the
//! executor.
//!
//! The *authoring* surface — the [`define_service!`] / [`sm_rule!`] macros and
//! the registry types [`ServiceSeed`] / [`ServiceDef`] — moved to the public
//! Rule SDK (`b2bua-sdk`, ADR-0016 slice 6) so an out-of-tree service crate has
//! no dependency on `b2bua`. The glue below stays here because it needs the
//! engine ([`ActionExecutor`], [`HandlerResult`]); it is re-exported through
//! `crate::rules` so in-tree call sites are unchanged.

use call::MachineId;

use crate::effects::HandlerResult;
use crate::event::CallEvent;

use super::actions::ActionExecutor;
use super::model::{RuleCall, RuleContext};

// Re-export the authoring registry types so `crate::rules::{ServiceDef,
// ServiceSeed}` and the macros' `$crate::rules::…` references resolve identically
// whether the rule was authored in-tree or out-of-crate.
pub use b2bua_sdk::service::{ServiceDef, ServiceSeed, Terminal};
use b2bua_sdk::model::RuleDefinition;

/// The engine's rule list: every service's state-gated rules (SERVICE_LAYER,
/// ranked above core) followed by the `core` defaults. With an empty service
/// list this is exactly `core` — composition is behaviour-preserving.
pub fn compose_rules(services: &[ServiceDef], core: Vec<RuleDefinition>) -> Vec<RuleDefinition> {
    let mut rules = Vec::new();
    for def in services {
        rules.extend((def.rules)());
    }
    rules.extend(core);
    rules
}

/// Run every service's `init` once at call setup and fold the returned seeds —
/// cursor + data backing + initial actions — through the normal executor/effects
/// pipeline (ADR-0016 X8). The cursor is keyed by the service id (== machine
/// id). A dormant service (`init` → `None`) is skipped. With an empty service
/// list this returns `result` unchanged.
pub fn seed_services(
    mut result: HandlerResult,
    services: &[ServiceDef],
    exec: &ActionExecutor,
    setup_event: &CallEvent,
    source_leg_id: &str,
    direction: call::Direction,
) -> HandlerResult {
    for def in services {
        let Some(seed) = (def.init)(&RuleCall::new(&result.call)) else {
            continue;
        };
        // 1) seed the cursor (the service id is its machine id),
        result
            .call
            .sm_cursors
            .insert(MachineId::new(def.id), seed.initial_state);
        // 2) install the data backing,
        (seed.data_write)(&mut result.call);
        // 3) fold the initial actions through the executor (no back-door write).
        if !seed.actions.is_empty() {
            let sub = {
                let ctx = RuleContext {
                    call: RuleCall::new(&result.call),
                    call_ref: &result.call.call_ref,
                    event: setup_event,
                    source_leg_id,
                    direction,
                    now_ms: exec.now_ms,
                    config: exec.config,
                };
                exec.execute(&seed.actions, &result.call, &ctx)
            };
            result.call = sub.call;
            result.effects.extend(sub.effects);
        }
    }
    result
}
