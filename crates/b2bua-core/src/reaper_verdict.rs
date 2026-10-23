//! The names of the call reaper's verdicts (ADR-0020): the synthetic event's
//! topic and outcomes the reaper sends and the CORE_LAYER rules match on.

/// The synthetic-event topic every reaper verdict carries.
pub const REAPER_TOPIC: &str = "reaper";
/// Sweep verdict: the call's last-touched stamp is older than the idle
/// threshold. Handled by the CORE_LAYER `reaper-stale` rule. Carries the
/// observed stamp as `payload.watermark` (the X5 confirm input).
pub const OUTCOME_STALE: &str = "stale";
/// Strike-1 verdict: a handler body for this call panicked or was aborted.
/// Handled by the CORE_LAYER `reaper-fatal-error` rule, which tears an Active
/// call down on the wire and forces a terminating one terminal.
pub const OUTCOME_FATAL: &str = "fatal-error";
/// Overflow verdict: the call's dispatch overflow reached its ceiling — more
/// must-run events arrive than the call consumes. Handled by the CORE_LAYER
/// `reaper-overflow` rule, which tears the live legs down on the wire;
/// re-sent each sweep on its own escalation ladder while the call stays
/// Active.
pub const OUTCOME_OVERFLOW: &str = "overflow";
/// Message-cap verdict: the call crossed its lifetime message cap (the work
/// bound). Handled by the CORE_LAYER `reaper-message-cap` rule: an Active
/// call is torn down on the wire, a terminating one forced terminal. Sent
/// once per call.
pub const OUTCOME_MESSAGE_CAP: &str = "message-cap";
/// Strike-2 verdict: the rules path itself failed (a second panic, or stale
/// verdicts that never took effect). Deliberately has NO rule — the router's
/// discharge branch forces the last persisted snapshot terminal and runs it
/// through `finalize → enforce → process_result` directly.
pub const OUTCOME_DISCHARGE: &str = "discharge";
