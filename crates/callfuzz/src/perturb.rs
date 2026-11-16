//! The perturbation any compiled call takes: jittered dwells, hostile INVITE
//! headers under the size budget, odd INFO bodies on the confirmed dialog.

use std::time::Duration;

use callshapes::plan::TRANSIT;
use scenario_harness::actor::{ActorCall, Barrier, Goal, GoalStep, LegPhase};
use scenario_harness::realcall::InvitePlan;
use sip_message::generators::InDialogMethod;

use crate::{payload, FuzzConfig, Rng};

/// INFOs one perturbed call carries, at most.
const MAX_INFOS: u64 = 2;

/// The bytes one header takes on the wire: name, `": "` and CRLF.
fn header_bytes(name: &str, value: &str) -> usize {
    name.len() + value.len() + 4
}

/// Perturb `call` in place, each change named in `notes`. With
/// `config.jitter_pct` percent, some goal delays grow by a drawn span; with
/// `config.payload_pct` percent, every INVITE gains up to four hostile headers
/// (one per name, compact forms counted as their long name, inside
/// `config.header_budget` unless `config.oversize`) and up to two parties
/// send an INFO with an odd body once both they and the caller are confirmed,
/// before their BYE. A call's draws are a function of `rng` alone.
pub fn perturb(rng: &mut Rng, call: &mut ActorCall, config: &FuzzConfig, notes: &mut Vec<String>) {
    if rng.chance(config.jitter_pct) {
        jitter(rng, call, notes);
    }
    if rng.chance(config.payload_pct) {
        for g in call.actors.iter_mut().flat_map(|a| a.goals.iter_mut()) {
            if let GoalStep::Invite { plan: Some(plan), .. } = &mut g.step {
                hostile_headers(rng, plan, config, notes);
            }
        }
        for _ in 0..rng.below(MAX_INFOS + 1) {
            odd_info(rng, call, config, notes);
        }
    }
}

/// Some goal delays grow by up to a drawn span.
fn jitter(rng: &mut Rng, call: &mut ActorCall, notes: &mut Vec<String>) {
    let span = *rng.pick(&[TRANSIT / 2, TRANSIT * 2, TRANSIT * 6, Duration::from_millis(1500)]);
    let span_ms = span.as_millis().max(1) as u64;
    for g in call.actors.iter_mut().flat_map(|a| a.goals.iter_mut()) {
        if rng.chance(60) {
            g.delay += Duration::from_millis(rng.below(span_ms));
        }
    }
    notes.push(format!("jitter<{span_ms}ms"));
}

/// The header name a compact form stands for (RFC 3261 §7.3.3), lowercased.
fn canonical(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "s" => "subject".to_string(),
        other => other.to_string(),
    }
}

/// Up to four drawn headers on `plan`, none repeating a name it holds.
fn hostile_headers(
    rng: &mut Rng,
    plan: &mut InvitePlan,
    config: &FuzzConfig,
    notes: &mut Vec<String>,
) {
    for _ in 0..1 + rng.below(4) {
        let (name, value, label) = payload::header(rng);
        if plan.headers.iter().any(|(h, _)| canonical(h) == canonical(&name)) {
            continue;
        }
        let used: usize = plan.headers.iter().map(|(h, v)| header_bytes(h, v)).sum();
        if !config.oversize && used + header_bytes(&name, &value) > config.header_budget {
            continue;
        }
        plan.headers.push((name, value));
        notes.push(format!("hdr:{label}"));
    }
}

/// One drawn party sends an INFO with an odd body on its confirmed dialog,
/// ahead of its BYE; a party with no BYE goal sends none.
fn odd_info(rng: &mut Rng, call: &mut ActorCall, config: &FuzzConfig, notes: &mut Vec<String>) {
    let idx = rng.below(call.actors.len() as u64) as usize;
    let caller = scenario_harness::actor::originating_role(&call.actors);
    let actor = &mut call.actors[idx];
    let Some(pos) =
        actor.goals.iter().position(|g| matches!(g.step, GoalStep::Bye | GoalStep::ByeWith { .. }))
    else {
        return;
    };
    let (content_type, body, label) = payload::body(rng, config.illegal);
    let mut headers = Vec::new();
    if rng.chance(50) {
        let (name, value, l) = payload::header(rng);
        headers.push((name, value));
        notes.push(format!("info-hdr:{l}"));
    }
    let sender = actor.role;
    let confirmed = Barrier::pred("fz_confirmed", move |s| {
        s.leg_at_least(caller, LegPhase::Confirmed) && s.leg(sender).phase() == LegPhase::Confirmed
    });
    let ended =
        Barrier::pred("fz_leg_ended", move |s| s.leg(sender).phase() == LegPhase::Terminated);
    let step = GoalStep::InDialog {
        method: InDialogMethod::Info,
        content_type: Some(content_type),
        body: Some(body),
        headers,
    };
    actor.goals.insert(pos, Goal::new(confirmed, step).unless(ended));
    notes.push(format!("info({sender}):{label}"));
}
