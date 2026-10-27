//! A rule's method column picks the rule for an in-dialog request: a BYE
//! selects `relay-bye`, and the method reads as spelled (RFC 3261 §7.1), so an
//! in-dialog `invite` is an extension method, which no `INVITE` rule takes.

use super::fixtures::*;
use super::*;
use b2bua_core::rule_fixture::context;

/// The in-dialog request of `method`, its CSeq `2 <method>`.
fn in_dialog(method: Method) -> SipRequest {
    let req = in_dialog_request(method.clone());
    let cseq = header::CSeq::new(2, method);
    req.thaw().set(cseq).freeze().expect("an in-dialog request")
}

fn accepts(matcher: &Match, request: SipRequest) -> bool {
    let mut call = test_call();
    confirm_a(&mut call);
    let event = CallEvent::sip(SipMessage::Request(request), "127.0.0.1:5060".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "a", &config);
    matcher.accepts_columns(&ctx)
}

#[test]
fn an_extension_invite_matches_no_invite_rule() {
    let lower = || in_dialog(Method::Other("invite".to_string()));
    assert_eq!(lower().cseq().method().as_str(), "invite");
    assert!(!accepts(&Match::request().methods(&["INVITE"]), lower()));
    assert!(accepts(&Match::request().methods(&["invite"]), lower()));
    assert!(accepts(&Match::request().methods(&["INVITE"]), in_dialog(Method::Invite)));
}

#[test]
fn in_dialog_bye_selects_relay_bye() {
    let mut call = test_call();
    // An in-dialog BYE (carries a To-tag) on the established call.
    confirm_a(&mut call);
    let bye = in_dialog_request(sip_message::Method::Bye);
    let event = CallEvent::sip(SipMessage::Request(bye), "127.0.0.1:5060".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "a", &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("relay-bye"));
}
