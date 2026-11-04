// ── default_sdp config → CreateLeg body_override (service fake-offer source) ──
//
// The `default_sdp` service parameter is a canned SDP a service sources to
// originate a deliberate *fake-offer* INVITE. The wiring is the existing
// `CreateLeg { body_override }` mechanism: the service passes
// `body_override: ctx.config.default_sdp.clone()` and `build_b_leg` stamps that
// body (+ `Content-Type: application/sdp`) onto the emitted b-leg INVITE.
// `default_sdp` is NEVER an automatic fallback — a normal reroute/failover
// `CreateLeg` (`body_override: None`) still relays the caller's own offer.
use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;
use sip_message::HeaderName;

#[test]
fn create_leg_sources_body_override_from_config_default_sdp() {
    let sdp = b"v=0\r\no=svc 42 42 IN IP4 10.0.0.9\r\ns=fake-offer\r\n\
                c=IN IP4 10.0.0.9\r\nt=0 0\r\nm=audio 50000 RTP/AVP 8\r\n"
        .to_vec();
    // A service authoring a fake-offer INVITE parks the canned SDP on config.
    let config = B2buaConfig { default_sdp: Some(sdp.clone()), ..Default::default() };

    let call = test_call();
    let event = CallEvent::sip(SipMessage::Request(invite()), "127.0.0.1:5060".parse().unwrap());
    let ctx = context(&call, &event, "a", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);

    // The service sources the fake offer from the config parameter — the whole
    // opt-in wiring. (A normal reroute passes `body_override: None` here, which
    // relays the caller's own offer; `default_sdp` never substitutes itself.)
    let create = RuleAction::CreateLeg {
        destination: ("10.0.1.5".into(), 5070), // IP literal → admission passes
        new_ruri: None,
        new_from: None,
        new_to: None,
        callback_context: None,
        body_override: config.default_sdp.clone().map(|sdp| Body::own(sdp, None)),
        header_updates: vec![],
        header_adds: vec![],
        kind: None,
        attempt: None,
    };
    let result = exec.execute(&[create], &call, &ctx);

    let invite_effect = result
        .effects
        .outbound
        .iter()
        .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "INVITE"))
        .expect("CreateLeg emits a b-leg INVITE");
    let inv = match &invite_effect.body {
        OutboundBody::Request(r) => r,
        _ => unreachable!(),
    };
    assert_eq!(
        &inv.body()[..],
        &sdp[..],
        "the b-leg INVITE carries the config default_sdp sourced via body_override"
    );
    assert_eq!(
        inv.raw(HeaderName::ContentType).next(),
        Some("application/sdp"),
        "the fake-offer INVITE advertises Content-Type: application/sdp"
    );
}
