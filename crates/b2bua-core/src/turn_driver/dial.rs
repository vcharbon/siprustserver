//! A caller's INVITE folded as the shell's initial turn folds it: the call
//! built (sampled when the driver says so), its record's rebuild of the
//! INVITE, the call refused 400 without a From tag or the driver's route
//! applied up to its admit, admitted and its services seeded, the turn
//! finalized, its obligations enforced, its lifecycle delivered and the turn
//! landed.

use std::net::SocketAddr;

use call::{Call, Direction, TerminationCause};
use sip_ids::TurnIds;
use sip_message::SipRequest;

use super::{landing, Driver, Session};
use crate::decision::route_fold::{self, Admitting};
use crate::effects::HandlerResult;
use crate::initial_invite::{build_initial_call, reject_call, setup_event};
use crate::landing::land;
use crate::owners::Owners;
use crate::rule_fixture::executor;
use crate::rules::invariants::{enforce, finalize, UnansweredCaller};
use crate::rules::lifecycle_delivery::{close_turn, TurnStart};
use crate::rules::relay::rebuild_a_leg_invite;
use crate::rules::seed_services;
use crate::turn_scope::TurnScope;

/// What the shell's initial turn makes of a caller's INVITE.
pub enum Dialling<'d> {
    /// The call is never born: its record does not rebuild the INVITE
    /// (ADR-0052 §11).
    Refused,
    /// The call's final, as the turn landed it: the INVITE carries no From
    /// tag (RFC 3261 §8.1.1.3), or the route was refused.
    Final(Box<HandlerResult>),
    /// bob's leg `b-1` dialled: the call to drive.
    Dialled(Box<Session<'d>>),
}

/// `invite` from alice folded through `driver`'s route at the instant 0 and
/// landed, or `None` when the call is never born.
pub(super) fn fold(driver: &Driver, invite: &SipRequest) -> Option<HandlerResult> {
    let config = &driver.config;
    let src = SocketAddr::from(([10, 0, 0, 9], 5060));
    let mut turn = TurnScope::of(TurnIds::fixed(0));
    let mut call = build_initial_call(invite, src, config, &mut turn.ids, 0);
    call.sampled = driver.sampled.then_some(true);
    let caller = rebuild_a_leg_invite(&call.a_leg_invite).ok()?;
    let born = call.clone();
    let handled = if call.a_leg_invite.from_tag.is_empty() {
        let reason = Some("Bad Request - missing From tag".into());
        let advertisement = &config.minted_final_advertisement;
        let cause = TerminationCause::Admission;
        let ids = &mut turn.ids;
        reject_call(call, &caller, 400, reason, None, &[], advertisement, ids, 0, cause)
    } else {
        routed(driver, call, &caller, &mut turn)
    };
    let unanswered = UnansweredCaller::Answer(&config.minted_final_advertisement);
    let finalized = finalize(handled);
    let enforced = enforce(&driver.obligations, &born, finalized, 0, unanswered, &mut turn.notes);
    let mut exec = executor(config, &mut turn);
    let closed = close_turn(&driver.rules, &TurnStart::unborn(), enforced, &mut exec);
    Some(land(closed, &landing(config), 0))
}

/// `call` routed as `driver`'s route says, admitted unless the route is
/// refused, its services seeded from the setup of `caller`'s INVITE.
fn routed(driver: &Driver, call: Call, caller: &SipRequest, turn: &mut TurnScope) -> HandlerResult {
    let config = &driver.config;
    let ids = &mut turn.ids;
    let result =
        match route_fold::up_to_admit(call, driver.route.clone(), caller, config, ids, 0, 0) {
            Admitting::Final(result) => *result,
            Admitting::Admit(admitting) => {
                route_fold::admitted(*admitting, caller, config, Owners::none(), ids, 0, 0)
            }
        };
    let setup = setup_event(&result.call, caller);
    let mut exec = executor(config, turn);
    seed_services(result, &driver.services, &mut exec, &setup, "a", Direction::FromA)
}
