//! The answers a party holds back for a drawn time or refuses: the ACK to a
//! 2xx, which the answerer re-sends until it arrives (RFC 3261 §13.3.1.4), so
//! the system under test meets a retransmitted 2xx and a confirmed dialog its
//! far end has not yet confirmed; the final to a renegotiation it takes,
//! held past the system's own requests or refused (§14.2: the session stays
//! as it was), so a relayed offer meets a slow or failed far end.

use std::time::Duration;

use scenario_harness::actor::answers::Answer;
use scenario_harness::actor::{originating_role, ActorCall};
use sip_message::DelayedAutomatic;

use crate::{FuzzConfig, Rng};

/// The longest drawn ACK hold: past the answerer's third 2xx re-send at T1 =
/// 500 ms, well inside its 64·T1 give-up (§13.3.1.4).
const ACK_HOLD_MAX_MS: u64 = 4000;

/// With `pct` percent, the caller holds the ACK of its establishing 2xx, or
/// of every 2xx it owes, for a drawn time.
pub fn hold_acks(rng: &mut Rng, call: &mut ActorCall, pct: u64, notes: &mut Vec<String>) {
    if !rng.chance(pct) {
        return;
    }
    let caller = originating_role(&call.actors);
    let Some(actor) = call.actors.iter_mut().find(|a| a.role == caller) else { return };
    let ms = 50 + rng.below(ACK_HOLD_MAX_MS - 50);
    let every = rng.chance(30);
    let hold = DelayedAutomatic::ack_after(ms);
    actor.delayed.push(if every { hold } else { hold.on_invite(0) });
    notes.push(format!("ack-hold({caller}){}:{ms}ms", if every { "*" } else { "" }));
}

/// The longest drawn hold of a renegotiation's final: inside the requester's
/// Timer B and F (64·T1, RFC 3261 §17.1.1.2, §17.1.2.2).
const ANSWER_HOLD_MAX_MS: u64 = 3000;

/// The finals a refused renegotiation draws: the offer refused, the server
/// failing (RFC 3261 §14.2, §21.5.1).
const REFUSALS: [u16; 2] = [488, 500];

/// One drawn way to answer a renegotiation: at once half the time, else held,
/// refused, or held then refused; with `may_refuse` false, never refused.
fn answer(rng: &mut Rng, may_refuse: bool) -> Answer {
    let pick = rng.below(8);
    let hold = Duration::from_millis(50 + rng.below(ANSWER_HOLD_MAX_MS - 50));
    let refuse = may_refuse.then_some(*rng.pick(&REFUSALS));
    match pick {
        0..=3 => Answer::default(),
        4..=5 => Answer { hold, refuse: None },
        6 => Answer { hold: Duration::ZERO, refuse },
        _ => Answer { hold, refuse },
    }
}

fn label(a: &Answer) -> String {
    match (a.hold.is_zero(), a.refuse) {
        (true, None) => "now".to_string(),
        (false, None) => format!("hold{}", a.hold.as_millis()),
        (true, Some(s)) => format!("{s}"),
        (false, Some(s)) => format!("hold{}-{s}", a.hold.as_millis()),
    }
}

/// With `config.hold_pct` percent, each party draws how it answers the
/// re-INVITEs and the UPDATEs it takes on its confirmed dialog.
pub fn hold_answers(
    rng: &mut Rng,
    call: &mut ActorCall,
    config: &FuzzConfig,
    notes: &mut Vec<String>,
) {
    if !rng.chance(config.hold_pct) {
        return;
    }
    for actor in &mut call.actors {
        let (reinvite, update) = (answer(rng, config.refuse), answer(rng, config.refuse));
        if reinvite.is_default() && update.is_default() {
            continue;
        }
        actor.answers.reinvite = reinvite;
        actor.answers.update = update;
        notes.push(format!(
            "answers({}):reinvite={},update={}",
            actor.role,
            label(&reinvite),
            label(&update)
        ));
    }
}

/// With `config.hold_pct` percent, the caller holds the PRACK of each reliable
/// provisional it takes for a drawn time up to `config.prack_hold_max_ms`, so
/// the provisional is re-sent, and a sibling fork's PRACK may cross the 2xx of
/// another (RFC 3261 §13.2.2.4).
pub fn hold_pracks(
    rng: &mut Rng,
    call: &mut ActorCall,
    config: &FuzzConfig,
    notes: &mut Vec<String>,
) {
    if !rng.chance(config.hold_pct) {
        return;
    }
    let caller = originating_role(&call.actors);
    let Some(actor) = call.actors.iter_mut().find(|a| a.role == caller) else { return };
    let ms = 50 + rng.below(config.prack_hold_max_ms.saturating_sub(50).max(1));
    actor.answers.prack = Duration::from_millis(ms);
    notes.push(format!("prack-hold({caller}):{ms}ms"));
}
