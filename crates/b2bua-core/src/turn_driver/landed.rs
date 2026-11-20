//! A driven turn's result landed as the shell lands it: its unheld answers
//! sent whole, as the shell's book sends them on their final's first arrival
//! (the driver holds no book, ADR-0063 §2), then the landing pass.

use std::time::Duration;

use crate::answer_deadline::AnswerBudgets;
use crate::config::B2buaConfig;
use crate::effects::HandlerResult;
use crate::landing::{land, Landing};
use crate::unheld_response::UnheldAnswer;

/// `result` landed at `now_ms` under `config`, its unheld answers sent behind
/// its own messages.
pub(super) fn landed(
    mut result: HandlerResult,
    config: &B2buaConfig,
    now_ms: i64,
) -> HandlerResult {
    let unheld = std::mem::take(&mut result.effects.unheld);
    result.effects.outbound.extend(unheld.into_iter().flat_map(UnheldAnswer::into_effects));
    land(result, &landing(config), now_ms)
}

/// The landing pass every driven turn takes.
fn landing(config: &B2buaConfig) -> Landing<'_> {
    Landing {
        config,
        budgets: AnswerBudgets {
            http: Duration::from_secs(5),
            admit: Duration::from_secs(1),
            failure: Duration::from_secs(5),
            release: Duration::from_secs(5),
        },
        refresh_period: Duration::from_secs(60),
    }
}
