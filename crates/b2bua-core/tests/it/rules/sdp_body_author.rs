//! A re-INVITE's body names its author: the peer whose session the dialog
//! carries continues it as written, the same bytes from this stack are restated
//! under that session (RFC 3264 §8).

use super::*;
use b2bua_core::effects::OutboundBody;

const BOB_V5: &str =
    "v=0\r\no=bob 202 5 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

fn reinvite_body(author: BodyAuthor) -> String {
    let config = B2buaConfig::default();
    let mut call = call_with_retained_answered_2xx();
    call.a_leg.sdp_session = call::LegSdpSession {
        sent_origin: Some("bob 202 1 IN IP4 192.0.2.2".into()),
        sent_media: vec!["audio 20000 RTP/AVP 0".into()],
        session_author: Some("b-1".into()),
        session_id: Some("202".into()),
        ..Default::default()
    };
    let event = CallEvent::sip(SipMessage::Request(invite()), "127.0.0.1:5060".parse().unwrap());
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor {
        owners: b2bua_core::owners::Owners::none(),
        config: &config,
        turn: &mut id_gen,
        now_ms: 0,
    };
    let reinvite = RuleAction::SendReinvite {
        leg_id: "a".into(),
        body: Some(Body::new(BOB_V5.as_bytes().to_vec(), None, author)),
        add_headers: vec![],
    };
    let result = exec.execute(&[reinvite], &call, &ctx);
    let effect = result.effects.outbound.first().expect("the re-INVITE leaves");
    let OutboundBody::Request(req) = &effect.body else { panic!("a request") };
    String::from_utf8_lossy(req.body()).into_owned()
}

#[test]
fn the_sessions_own_author_is_sent_as_written_and_the_stack_is_restated() {
    assert_eq!(reinvite_body(BodyAuthor::Leg("b-1".into())), BOB_V5);
    assert!(
        reinvite_body(BodyAuthor::Stack).contains("o=bob 202 2 IN IP4 192.0.2.2\r\n"),
        "the stack's description continues the session one version up"
    );
}
