//! The caller's INVITE as the decision's route meets it (ADR-0051 §5: every
//! caller reaches it before any dialog exists): the decisions a case routes
//! the INVITE under, and what an INVITE folded through one of them may
//! leave — the call refused, or a b-leg INVITE that reads back and refreezes
//! to itself. The caller's body, Content-Type and entity headers are mixed
//! with the decision's statements and parts at the mint.

use b2bua_core::decision::{BodyUpdate, HeaderUpdate, RouteDecision, SipDestination};
use b2bua_core::effects::OutboundBody;
use b2bua_core::turn_driver::{Dialling, Driver, Session};
use call::features::StatedHeaders;
use sip_message::parser::custom::CustomParser;
use sip_message::{MultipartPart, SipMessage, SipParser, SipRequest};

use super::drive;

/// A decision a case routes the caller's INVITE under: the driver whose
/// route it is.
pub struct Decision {
    pub name: &'static str,
    pub driver: Driver,
    /// Whether the route dials: its destination is one the call may reach.
    pub dials: bool,
}

const HELD: &str = "v=0\r\no=lane 2 2 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=inactive\r\n";

fn statements(entries: &[(&str, HeaderUpdate)]) -> b2bua_core::decision::SipHeaderUpdates {
    entries.iter().map(|(name, update)| (name.to_string(), update.clone())).collect()
}

/// The decisions every case routes under: plain, with stated headers, with
/// parts attached beside the caller's body, with the body replaced, and
/// toward a destination the admission refuses.
pub fn decisions() -> Vec<Decision> {
    let routed = |edit: fn(&mut RouteDecision)| Driver::new().with_route(edit);
    vec![
        Decision { name: "plain", driver: Driver::new(), dials: true },
        Decision { name: "stated-headers", driver: routed(stated), dials: true },
        Decision { name: "attached-parts", driver: routed(attached), dials: true },
        Decision { name: "body-replaced", driver: routed(replaced), dials: true },
        Decision { name: "refused-destination", driver: routed(unlisted), dials: false },
    ]
}

/// `route` stating headers on the b-leg INVITE and on the messages it
/// relays.
fn stated(route: &mut RouteDecision) {
    route.update_headers = Some(statements(&[
        ("Subject", HeaderUpdate::line("routed")),
        ("Priority", HeaderUpdate::Remove),
        ("Organization", HeaderUpdate::Add(vec!["lane".into()])),
    ]));
    route.features.stated_headers = Some(StatedHeaders {
        launched_invite: statements(&[("Call-Info", HeaderUpdate::line("<http://example.com/i>"))]),
        originator_finals: statements(&[(
            "Warning",
            HeaderUpdate::line("399 example.com \"lane\""),
        )]),
        every_message: statements(&[
            ("User-Agent", HeaderUpdate::line("lane")),
            ("Accept-Language", HeaderUpdate::Add(vec!["en".into()])),
        ]),
        ..Default::default()
    });
}

/// `route` attaching a part beside the caller's body.
fn attached(route: &mut RouteDecision) {
    route.update_body = BodyUpdate::AttachParts(vec![MultipartPart {
        content_type: "application/pidf+xml".into(),
        headers: vec![
            ("Content-ID".into(), "<lane@example.com>".into()),
            ("Content-Disposition".into(), "by-reference".into()),
        ],
        payload: b"<presence entity=\"sip:alice@example.com\"/>".to_vec(),
    }]);
}

/// `route` replacing the caller's body with a held offer.
fn replaced(route: &mut RouteDecision) {
    route.update_body = BodyUpdate::Replace(HELD.into());
}

/// `route` stating headers toward a destination the admission refuses.
fn unlisted(route: &mut RouteDecision) {
    stated(route);
    route.destination = SipDestination::new("unlisted.invalid", 5070);
}

/// The status of the final `result` sends the caller, if any.
fn caller_final(result: &b2bua_core::effects::HandlerResult) -> Option<u16> {
    result.effects.outbound.iter().find_map(|e| match &e.body {
        OutboundBody::Response(r) if e.leg_id.as_deref() == Some("a") && r.status() >= 200 => {
            Some(r.status())
        }
        _ => None,
    })
}

/// `raw` from alice dialled under `decision`: the dialled call, or `None`
/// when `raw` is no INVITE or the call is refused. Panics on what no INVITE
/// may leave: a dialling route refused with the 500 of a b-leg INVITE that
/// did not build, a refused call with no final for the caller, or a b-leg
/// INVITE that does not read back or refreeze to itself.
pub fn dial_checked<'d>(raw: &[u8], decision: &'d Decision) -> Option<Session<'d>> {
    let invite = drive::invite_of(raw)?;
    match decision.driver.dial_invite(&invite) {
        Dialling::Refused => None,
        Dialling::Final(result) => {
            let status = caller_final(&result);
            assert!(status.is_some(), "{}: a refused call answers its caller", decision.name);
            assert!(
                !decision.dials || status != Some(500),
                "{}: the b-leg INVITE did not build from the caller's",
                decision.name
            );
            None
        }
        Dialling::Dialled(dialled) => {
            assert!(decision.dials, "{}: the route dials nothing", decision.name);
            refreezes(drive::bob_invite(&dialled), decision.name);
            Some(*dialled)
        }
    }
}

/// `invite` reads back as the request it is, and its reading refreezes to the
/// same bytes.
fn refreezes(invite: &SipRequest, name: &str) {
    let image = invite.image();
    let read = match CustomParser::new().parse(image) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("{name}: the b-leg INVITE reads back: {other:?}"),
    };
    let frozen = read.thaw().freeze().unwrap_or_else(|e| panic!("{name}: it refreezes: {e}"));
    assert_eq!(frozen.image(), image, "{name}: the b-leg INVITE refreezes to itself");
}
