//! The `transfer` callflow machine (ADR-0016): `TransferPhase` is the declared
//! machine, its cursor a read-only *projection* of the authoritative
//! `Call.transfer.phase` (see [`project_cursor`], mirroring the `global-call`
//! projection). Each rule is gated by `active_states`; the `transitions` it
//! declares are the diagram edges (the cursor is moved by the projection in
//! `finalize`, so they are documentation, never enforced). Handlers write
//! `Call.transfer.phase` via `SetTransfer`, which the projection mirrors.
//!
//! The rules live phase-per-file — [`authorizing`], [`c_ringing`],
//! [`realign`] — with the cross-phase request guards and watchdog in
//! [`guards`]; the `define_service!` list below is the registration index.

use b2bua_sdk::define_service;
use call::{Call, StateLabel, TransferPhase, TransferState};

use crate::rules::model::{RuleContext, RuleDefinition};

mod authorizing;
mod c_ringing;
mod guards;
mod realign;

// ── Timer-id minting (must match `ActionExecutor::schedule`) ─────────────────
// `schedule` builds `format!("{:?}", t)` (no leg) / `format!("{:?}:{}", t, leg)`.
// Mint cancel ids from the same recipe so they can never drift (CLAUDE.md timer-
// aliasing hazard).
fn timer_id(t: call::TimerType, leg: Option<&str>) -> String {
    match leg {
        Some(l) => format!("{t:?}:{l}"),
        None => format!("{t:?}"),
    }
}

fn state<'a>(ctx: &'a RuleContext<'a>) -> Option<&'a TransferState> {
    ctx.call.transfer_state()
}

// `init` stays dormant (`None`): transfer is triggered by an in-dialog REFER
// mid-call (the machine-less seed rules in `seed`), not at INVITE setup. The
// cursor first appears when the seed installs the slice.
define_service! {
    id: "transfer",
    machine: TRANSFER_MACHINE,
    states: Phase { ReferAuthorizing, CRinging, CRealigning, ARealigning },
    init: |_call| None,
    rules: [
        guards::reject_second_refer(),
        authorizing::http_reject(),
        authorizing::http_allow(),
        authorizing::http_timeout(),
        c_ringing::c_1xx_to_notify(),
        c_ringing::c_200_initial(),
        c_ringing::c_fail_initial(),
        c_ringing::c_no_answer(),
        realign::c_realign_200(),
        realign::c_realign_fail(),
        realign::c_realign_timeout(),
        guards::c_glare_reinvite(),
        realign::a_realign_200(),
        realign::a_realign_fail(),
        realign::a_realign_timeout(),
        guards::a_glare_reinvite(),
        guards::overall_timeout(),
        guards::b_in_realign_reject(),
    ],
}

/// The machine-gated service rules, kept under the pre-`define_service!` name
/// for `default_rules()` (the engine runs them via the flat rule list; the
/// generated `rules()` is the source).
pub fn transfer_rules() -> Vec<RuleDefinition> {
    rules()
}

/// The `transfer` service descriptor — registered in the doc-generator registry
/// (`b2bua-runner::compose_services`) so `docs/sm/transfer.md` is generated from
/// the same declared `active_states`/`transitions` the engine gates on.
pub fn transfer_service_def() -> crate::rules::ServiceDef {
    service_def()
}

/// Project the authoritative `Call.transfer.phase` into the `transfer` machine
/// cursor (ADR-0016), mirroring the `global-call` projection: the cursor is a
/// read-only view the machine-gated rules select on, while `Call.transfer` stays
/// the single source of truth. Called from `invariants::finalize`. Clearing the
/// slice removes the cursor, deactivating the machine (so post-transfer relay is
/// no longer intercepted by the glare/realign rules).
pub fn project_cursor(call: &mut Call) {
    match call.transfer.as_ref().map(|t| t.phase) {
        Some(p) => {
            call.sm_cursors.insert(TRANSFER_MACHINE, phase_label(p));
        }
        None => {
            call.sm_cursors.remove(&TRANSFER_MACHINE);
        }
    }
}

fn phase_label(p: TransferPhase) -> StateLabel {
    match p {
        TransferPhase::ReferAuthorizing => Phase::ReferAuthorizing.label(),
        TransferPhase::CRinging => Phase::CRinging.label(),
        TransferPhase::CRealigning => Phase::CRealigning.label(),
        TransferPhase::ARealigning => Phase::ARealigning.label(),
    }
}
