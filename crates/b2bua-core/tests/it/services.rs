//! The services the driven tests inject into a driver's rule chain, each
//! acting on an internal event a test sends.

use b2bua_core::rules::{Match, RuleAction, RuleDefinition, RuleHandleResult, SERVICE_LAYER};
use b2bua_core::turn_driver::Step;
use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::Body;
use call::{Direction, LegKind};

/// The service's topic: `payload.leg` is re-INVITEd, offering
/// `payload.offer` when it is stated.
const REINVITE: &str = "test-reinvite";
/// The service's topic: `payload.leg` is released with a BYE.
const RELEASE: &str = "test-release";

/// A service re-INVITing or releasing a leg on its internal events.
pub(crate) fn reinvite_release() -> RuleDefinition {
    RuleDefinition::core(
        "test-reinvite-release",
        SERVICE_LAYER,
        &[],
        Match::internal_event(),
        |ctx| {
            let CallEvent::InternalEvent { topic, payload, .. } = ctx.event else { return None };
            let leg_id = payload.get("leg")?.as_str()?.to_string();
            let action = match topic.as_str() {
                REINVITE => {
                    let offer = payload.get("offer").and_then(|o| o.as_str());
                    let body = offer.map(|o| Body::own(o.as_bytes().to_vec(), None));
                    RuleAction::SendReinvite { leg_id, body, add_headers: vec![] }
                }
                RELEASE => RuleAction::DestroyLeg { leg_id, headers: vec![] },
                _ => return None,
            };
            Some(RuleHandleResult::new(vec![action]))
        },
    )
}

/// The service's event of `topic` on `leg`, with `payload`.
fn asks(topic: &'static str, leg: &'static str, payload: serde_json::Value) -> Step {
    Step::Internal { leg, topic, outcome: "ok", payload }
}

/// The service re-INVITEs `leg`, offering `offer` if any.
pub(crate) fn reinvites(leg: &'static str, offer: Option<&str>) -> Step {
    asks(REINVITE, leg, serde_json::json!({ "leg": leg, "offer": offer }))
}

/// The service releases `leg`.
pub(crate) fn releases(leg: &'static str) -> Step {
    asks(RELEASE, leg, serde_json::json!({ "leg": leg }))
}

/// The service's topic: a media leg is dialled toward the media server.
const MEDIA: &str = "test-media";

/// A service dialling a media leg on its internal event, and showing the
/// caller that leg's early media as each of its provisionals arrives.
pub(crate) fn early_media() -> [RuleDefinition; 2] {
    let dials = RuleDefinition::core(
        "test-media-dials",
        SERVICE_LAYER,
        &[],
        Match::internal_event().topic(MEDIA),
        |_| {
            Some(RuleHandleResult::new(vec![RuleAction::CreateLeg {
                destination: ("127.0.0.1".into(), 5072),
                new_ruri: Some("sip:media@127.0.0.1:5072".into()),
                new_from: None,
                new_to: None,
                callback_context: None,
                body_override: None,
                header_updates: vec![],
                header_adds: vec![],
                kind: Some(LegKind::Media),
                attempt: None,
            }]))
        },
    );
    let shows = RuleDefinition::core(
        "test-media-shows",
        SERVICE_LAYER,
        &[],
        Match::response().method("INVITE").status_class(1).direction(Direction::FromB),
        |ctx| {
            let media = ctx.call.b_legs().iter().find(|l| l.leg_id == ctx.source_leg_id)?;
            if call::helpers::leg_kind(media) != LegKind::Media {
                return None;
            }
            let body = Body::from_leg(ctx.response()?.body().to_vec(), ctx.source_leg_id);
            Some(RuleHandleResult::new(vec![RuleAction::SendProvisionalToLeg {
                leg_id: ctx.call.a_leg().leg_id.clone(),
                status: 183,
                reason: "Session Progress".into(),
                body: Some(body),
                to_tag: None,
                p_early_media: None,
            }]))
        },
    );
    [dials, shows]
}

/// The service dials its media leg.
pub(crate) fn plays_media() -> Step {
    asks(MEDIA, "a", serde_json::json!({}))
}
