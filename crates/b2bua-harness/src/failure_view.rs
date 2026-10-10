//! What a harness B2BUA adds to its run's failure dump
//! ([`scenario_harness::FailureView`]): the first model-invariant violation a
//! turn met out of the scenario's sight, as the run's cause, and the timer
//! ledger of every live call. Its per-call rule trace reaches the dump through
//! the trace plane, which [`trace_every_call`] opens for every call.

use std::sync::Arc;

use b2bua::trace::{install_process_traces_unless_installed, CallTraces};
use scenario_harness::{DumpSection, FailureView};
use sip_clock::Clock;

use crate::model_gate;

/// How many calls a harness process traces at once.
const TRACED_CALLS_MAX: usize = 1_000;

/// Trace every call this process serves, unless a test installed its own gate
/// first: the per-call `sip.in`/`rule.fired`/`rule.transition`/
/// `call.transition` facts (ADR-0026) then reach a failing run's dump.
/// Process-wide, so exact under a runner that gives each test its own process.
pub fn trace_every_call() {
    install_process_traces_unless_installed(CallTraces::sample_every_call(TRACED_CALLS_MAX));
}

/// The live calls of one B2BUA, read through the core's probe.
pub(crate) type LiveCalls = Arc<dyn Fn() -> Option<Vec<call::Call>> + Send + Sync>;

/// One B2BUA's view of a failing run.
pub(crate) struct B2buaFailureView {
    pub name: String,
    pub calls: LiveCalls,
    pub clock: Clock,
    /// Where this B2BUA's run starts in the process's violation record.
    pub since: model_gate::Mark,
}

impl FailureView for B2buaFailureView {
    fn cause(&self) -> Option<String> {
        model_gate::first_violation(self.since)
    }

    fn sections(&self) -> Vec<DumpSection> {
        let title = format!("{} timer ledger per live call", self.name);
        let Some(calls) = (self.calls)() else {
            return vec![DumpSection { title, lines: vec!["(the B2BUA is gone)".to_string()] }];
        };
        vec![DumpSection { title, lines: timer_ledger(&calls, self.clock.now_ms()) }]
    }
}

/// The armed timers of each call, one line each, deadlines relative to
/// `now_ms`.
fn timer_ledger(calls: &[call::Call], now_ms: i64) -> Vec<String> {
    if calls.is_empty() {
        return vec!["(no live call)".to_string()];
    }
    let mut lines = Vec::new();
    for c in calls {
        lines.push(format!(
            "call {} (Call-ID {}) {:?}, {} timer(s)",
            c.call_ref,
            c.a_leg.call_id,
            c.state,
            c.timers.len()
        ));
        let mut timers: Vec<_> = c.timers.iter().collect();
        timers.sort_by_key(|t| t.fire_at);
        for t in timers {
            let due = t.fire_at - now_ms;
            let when = if due >= 0 {
                format!("fires in {}ms", due)
            } else {
                format!("overdue by {}ms", -due)
            };
            let (timer_type, scope) = (t.timer_type(), &t.key.scope);
            lines.push(format!("  {timer_type:?} {when}, {scope:?}, gen {}", t.generation));
        }
    }
    lines
}
