//! A send's addresses: its tier-2 URIs with their parameters, and its stated
//! Contact.

use super::*;
use pivot_schema::msg::PositionalRef;

#[test]
fn a_send_carries_its_frozen_headers_its_tier_two_uris_their_params_and_the_lane_s_own_headers() {
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let resolver = Resolver::new(&state, &bindings);
    let msg = MsgSpec {
        method: Some("INVITE".into()),
        ruri: Some(Ref::Positional(PositionalRef {
            pos: "called[0][0]".into(),
            form: Some("trunk-composed".into()),
            addr: None,
            params: Some("user=phone".into()),
        })),
        from: Some(Ref::Positional(PositionalRef {
            pos: "caller".into(),
            form: Some("private".into()),
            addr: None,
            params: None,
        })),
        headers: vec![Header {
            name: "X-Vendor-Orig".into(),
            value: "sbc.111".into(),
            class: None,
        }],
        ..MsgSpec::default()
    };
    let rendered = render(
        &msg,
        "step \"s1\"",
        &Context {
            resolver: &resolver,
            config: &config(),
            effects: &StepEffects::default(),
            composer: &TestComposer,
            base_dir: Path::new("."),
            media: &Booking::new("127.0.0.1", 40000),
            leg: "A",
            mscml: &MscmlRequests::default(),
            call_headers: &BTreeMap::new(),
        },
    )
    .expect("the message composes");
    assert_eq!(rendered.ruri.as_deref(), Some("sip:+1999999900004@10.0.0.9:5080;user=phone"));
    assert_eq!(rendered.from.as_deref(), Some("sip:0009001@lane.invalid"));
    let names: Vec<&str> = rendered.template.headers().iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, ["X-Vendor-Orig", "X-Lane"], "the lane's own header rides last");
    assert_eq!(rendered.template.method(), Some(&Method::Invite));
}

/// A stated Contact's display name and user part resolve their accessors; the
/// stack writes them around the bound socket.
#[test]
fn a_stated_contact_resolves_its_accessors() {
    let state = RunState::new();
    let bindings = IdentityBindings::new().bind("caller", "e164", "+15556000777");
    let resolver = Resolver::new(&state, &bindings);
    let msg: MsgSpec = serde_json::from_str(
        r#"{ "method": "INVITE", "contact": { "display": "Front Desk", "user": "${num:caller:e164}" } }"#,
    )
    .expect("a message");
    let rendered = render(
        &msg,
        "step \"s1\"",
        &Context {
            resolver: &resolver,
            config: &config(),
            effects: &StepEffects::default(),
            composer: &TestComposer,
            base_dir: Path::new("."),
            media: &Booking::new("127.0.0.1", 40000),
            leg: "A",
            mscml: &MscmlRequests::default(),
            call_headers: &BTreeMap::new(),
        },
    )
    .expect("the message composes");
    assert_eq!(rendered.contact_display.as_deref(), Some("Front Desk"));
    assert_eq!(rendered.contact_user.as_deref(), Some("+15556000777"));
}
