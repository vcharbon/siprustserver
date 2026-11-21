//! Where a call's offer rides and how its bodies are framed.
//!
//! The caller's INVITE makes the offer, or makes none (RFC 3261 §13.2.1), bare
//! or carrying a body that is no session description; the callee then offers
//! in its first reliable provisional or its 2xx, and the caller answers in the
//! PRACK or the ACK (RFC 3262 §5, RFC 3261 §13.2.2.4). Each party frames every
//! description it sends bare or in a multipart body (RFC 5621), its other
//! parts optional to handle.

use scenario_harness::actor::framing::{Extra, Framing, NoOffer};
use scenario_harness::actor::{originating_role, ActorCall};

use crate::{FuzzConfig, Rng};

/// Where the caller's INVITE puts the offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferMode {
    /// The INVITE carries it.
    InInvite,
    /// The INVITE carries no body.
    Delayed,
    /// The INVITE carries a body that is no description.
    DelayedWithBody(NoOffer),
}

fn extra(rng: &mut Rng) -> Extra {
    *rng.pick(&Extra::ALL)
}

/// One drawn offer mode: the INVITE's offer three times in ten, no body on
/// it half the time, a body that is no description on the rest.
pub fn offer_mode(rng: &mut Rng) -> OfferMode {
    match rng.below(10) {
        0..=2 => OfferMode::InInvite,
        3..=7 => OfferMode::Delayed,
        _ if rng.chance(50) => OfferMode::DelayedWithBody(NoOffer::Bare(extra(rng))),
        _ => OfferMode::DelayedWithBody(NoOffer::Mixed(extra(rng), extra(rng))),
    }
}

/// One drawn framing: bare four times in ten, else a multipart variant.
pub fn framing(rng: &mut Rng) -> Framing {
    match rng.below(10) {
        0..=3 => Framing::Bare,
        4..=5 => Framing::Mixed { extra: extra(rng), sdp_first: rng.chance(50) },
        6..=7 => Framing::Alternative { extra: extra(rng) },
        _ => Framing::Nested { extra: extra(rng) },
    }
}

/// With `config.offer_pct` percent, draw the caller's offer mode and each
/// party's framing on `call`, each change named in `notes`. Only a caller
/// whose INVITE offers is given another offer mode.
pub fn draw_media(
    rng: &mut Rng,
    call: &mut ActorCall,
    config: &FuzzConfig,
    notes: &mut Vec<String>,
) {
    if !rng.chance(config.offer_pct) {
        return;
    }
    let caller = originating_role(&call.actors);
    for actor in &mut call.actors {
        if actor.role == caller && actor.media.makes_offer() {
            let mode = offer_mode(rng);
            match mode {
                OfferMode::InInvite => {}
                OfferMode::Delayed => actor.media = actor.media.delayed(None),
                OfferMode::DelayedWithBody(body) => actor.media = actor.media.delayed(Some(body)),
            }
            notes.push(format!("offer:{}", label(mode)));
        }
        let f = framing(rng);
        if f != Framing::Bare {
            actor.media = actor.media.framed(f);
            notes.push(format!("frame({}):{}", actor.role, f.label()));
        }
    }
}

fn label(mode: OfferMode) -> String {
    match mode {
        OfferMode::InInvite => "invite".to_string(),
        OfferMode::Delayed => "delayed".to_string(),
        OfferMode::DelayedWithBody(body) => format!("delayed-{}", body.label()),
    }
}
