//! The INVITE the stack originates toward a media leg: `100rel` is never
//! offered there, whatever the strategy (no service acknowledges a media leg's
//! reliable provisionals, RFC 3262 §4), and the originator's `Accept` stays
//! behind: it states what the originator takes, and a media leg answers the
//! service that dialled it; a destination leg carries the originator's lines.

use b2bua::config::B2buaConfig;
use b2bua::effects::{HandlerResult, OutboundBody};
use b2bua::initial_invite::build_initial_call;
use b2bua::rules::{ActionExecutor, RuleAction, RuleCall, RuleContext};
use b2bua_sdk::event::CallEvent;
use call::features::{RelayFirst18xStrategy, RelayFirst18xTo180Feature};
use call::{Direction, LegKind};
use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::{HeaderName, SipHeader, SipMessage, SipRequest, SipStr};
use sip_txn::IdGen;

fn uri_of(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

/// The caller's INVITE, offering `100rel` and stating an `Accept`.
fn invite() -> SipRequest {
    let line = |name: &str, value: &str| SipHeader { name: name.into(), value: value.into() };
    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: Some(uri_of("sip:bob@127.0.0.1:5070")),
        call_id: "media@alice".into(),
        from: Some(
            header::From::from_uri(uri_of("sip:alice@host")).with_tag(SipStr::from_static("atag")),
        ),
        to: Some(header::To::from_uri(uri_of("sip:bob@host"))),
        cseq: 1,
        via: Some(Via::udp("127.0.0.1", 5060).with_branch(SipStr::from_static("z9hG4bKmedia"))),
        contact: Some(header::Contact::from_uri(
            Uri::sip_user("alice", "127.0.0.1").with_port(5060),
        )),
        max_forwards: Some(70),
        body: b"v=0\r\n".to_vec(),
        content_type: None,
        extra_headers: vec![
            line("Supported", "100rel, path"),
            line("Accept", "application/sdp, application/isup"),
            line("Allow", "INVITE, ACK, BYE"),
        ],
    };
    generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts)
}

/// One `create-leg` toward a leg of `kind` on a call under `strategy`.
fn originate(
    config: &B2buaConfig,
    kind: Option<LegKind>,
    strategy: Option<RelayFirst18xStrategy>,
) -> HandlerResult {
    let src = "127.0.0.1:5060".parse().unwrap();
    let mut call = build_initial_call(&invite(), src, config, &IdGen::seeded(1), 0);
    if let Some(strategy) = strategy {
        let features = call.features.get_or_insert_with(b2bua::decision::default_platform_features);
        features.relay_first_18x_to_180 =
            Some(RelayFirst18xTo180Feature { strategy, messages: Default::default() });
    }
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Request(invite())),
        src,
        matched_client_txn: false,
    };
    let id_gen = IdGen::seeded(2);
    let exec = ActionExecutor {
        config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config,
        discharged: None,
    };
    let leg = RuleAction::CreateLeg {
        destination: ("127.0.0.1".into(), 5090),
        new_ruri: None,
        new_from: None,
        new_to: None,
        no_answer_timeout_sec: None,
        callback_context: None,
        body_override: None,
        header_updates: vec![],
        header_adds: vec![],
        kind,
    };
    exec.execute(&[leg], &call, &ctx)
}

/// The originated INVITE's lines of `name`.
fn sent(result: &HandlerResult, name: HeaderName) -> Vec<String> {
    let invite = result
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            OutboundBody::Request(r) => Some(r.clone()),
            _ => None,
        })
        .expect("the leg's INVITE");
    invite.raw(name).map(str::to_string).collect()
}

fn config() -> B2buaConfig {
    B2buaConfig { worker_allowed_target_suffixes: vec!["*".into()], ..B2buaConfig::default() }
}

#[test]
fn a_media_leg_is_offered_no_100rel_under_any_strategy() {
    let config = config();
    for strategy in [None, Some(RelayFirst18xStrategy::FakePrack)] {
        let result = originate(&config, Some(LegKind::Media), strategy);
        let supported = sent(&result, HeaderName::Supported).join(", ");
        assert!(!supported.contains("100rel"), "{strategy:?}: {supported}");
        assert!(supported.contains("path"), "the other tags ride: {supported}");
        assert!(sent(&result, HeaderName::Require).iter().all(|r| !r.contains("100rel")));
    }
}

#[test]
fn a_media_leg_is_told_none_of_the_originators_accept() {
    let result = originate(&config(), Some(LegKind::Media), None);
    assert!(sent(&result, HeaderName::Accept).is_empty());
    assert_eq!(sent(&result, HeaderName::Allow), ["INVITE, ACK, BYE"]);
}

#[test]
fn a_destination_leg_carries_the_originators_lines() {
    let result = originate(&config(), None, None);
    assert_eq!(sent(&result, HeaderName::Accept), ["application/sdp, application/isup"]);
    assert!(sent(&result, HeaderName::Supported).join(", ").contains("100rel"));
}
