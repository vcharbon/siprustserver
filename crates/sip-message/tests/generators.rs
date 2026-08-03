//! Unit tests for the generators — correct-by-default message construction.
//! Port of `tests/sip/generators.test.ts`.

use sip_message::generators::{
    extract_non_structural_headers, generate_ack_for_2xx, generate_ack_for_non_2xx,
    generate_cancel, generate_in_dialog_request, generate_out_of_dialog_request, generate_response,
    GenerateAckFor2xxOpts, GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts,
    CapabilitySet, GenerateResponseOpts, InDialogMethod, InviteClientTransactionHandle,
    OutOfDialogMethod, StackDialog,
};
use sip_message::header::{
    self, Event, HeaderName, HeaderValue, MediaType, ParamValue, RAck, SubscriptionState, Uri, Via,
};
use sip_message::method::Method;
use sip_message::{hydrate_request, SipHeader, SipMessage, SipRequest, SipStr};

fn name(header: &str) -> HeaderName {
    HeaderName::of(&SipStr::owned(header))
}

/// First value of one header.
fn first_value<'a>(headers: &'a [SipHeader], header: &str) -> Option<&'a str> {
    let named = name(header);
    headers.iter().find(|h| named.matches(&h.name)).map(|h| h.value.as_str())
}

/// Every value of one header, in wire order.
fn all_values<'a>(headers: &'a [SipHeader], header: &str) -> Vec<&'a str> {
    let named = name(header);
    headers.iter().filter(|h| named.matches(&h.name)).map(|h| h.value.as_str()).collect()
}

fn hdr(name: &str, value: &str) -> SipHeader {
    SipHeader { name: name.to_string().into(), value: value.to_string().into() }
}

fn uri_of(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

fn token(value: &str) -> ParamValue {
    ParamValue::Token(SipStr::owned(value))
}

fn from_hdr(uri: &str, tag: &str) -> header::From {
    header::From::from_uri(uri_of(uri)).with_tag(SipStr::owned(tag))
}

fn to_hdr(uri: &str) -> header::To {
    header::To::from_uri(uri_of(uri))
}

fn via() -> Via {
    Via::udp("10.0.0.1", 5060).with_branch("z9hG4bKtest00000000")
}

fn via_with_params() -> Via {
    via().with_param("cr", token("cref1")).with_param("lg", token("a"))
}

fn contact_uri() -> Uri {
    Uri::sip_user("b2bua", "10.0.0.1").with_port(5060)
}

fn contact() -> header::Contact {
    header::Contact::from_uri(contact_uri())
}

fn contact_with_params() -> header::Contact {
    header::Contact::from_uri(
        contact_uri().with_param("callRef", token("cref1")).with_param("leg", token("a")),
    )
}

fn dialog() -> StackDialog {
    StackDialog {
        call_id: "call-bleg-1".to_string(),
        local_tag: "b2bua-local".to_string(),
        remote_tag: "bob-remote".to_string(),
        local_uri: "sip:b2bua@10.0.0.1:5060".to_string(),
        remote_uri: "sip:bob@192.0.2.20:5060".to_string(),
        remote_target: "sip:bob@192.0.2.20:5060".to_string(),
        local_cseq: 100,
        route_set: vec![],
    }
}

fn sdp_body() -> Vec<u8> {
    [
        "v=0",
        "o=- 0 0 IN IP4 10.0.0.1",
        "s=-",
        "c=IN IP4 10.0.0.1",
        "t=0 0",
        "m=audio 20000 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "",
    ]
    .join("\r\n")
    .into_bytes()
}

fn make_a_leg_invite() -> SipRequest {
    let body = sdp_body();
    let headers = vec![
        hdr("Via", "SIP/2.0/UDP atlanta.example.com:5060;branch=z9hG4bKalice"),
        hdr("Max-Forwards", "70"),
        hdr("From", "\"Alice\" <sip:alice@atlanta.example.com>;tag=alice-tag"),
        hdr("To", "<sip:bob@biloxi.example.com>"),
        hdr("Call-ID", "call-aleg-1"),
        hdr("CSeq", "1 INVITE"),
        hdr("Contact", "<sip:alice@atlanta.example.com:5060>"),
        hdr("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS"),
        hdr("Supported", "replaces, 100rel"),
        hdr("P-Asserted-Identity", "<sip:alice@atlanta.example.com>"),
        hdr("Content-Type", "application/sdp"),
        hdr("Content-Length", &body.len().to_string()),
    ];
    hydrate_request("INVITE", "sip:bob@biloxi.example.com", headers, body).expect("a-leg hydrates")
}

fn invite_handle() -> InviteClientTransactionHandle {
    let headers = vec![
        hdr("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKinvite123;cr=cref1;lg=b-1"),
        hdr("Max-Forwards", "70"),
        hdr("From", "<sip:b2bua@10.0.0.1:5060>;tag=b2bua-local"),
        hdr("To", "<sip:bob@192.0.2.20:5060>"),
        hdr("Call-ID", "call-bleg-1"),
        hdr("CSeq", "42 INVITE"),
        hdr("Contact", "<sip:b2bua@10.0.0.1:5060;callRef=cref1;leg=b-1>"),
        hdr("Content-Length", "0"),
    ];
    let invite = hydrate_request("INVITE", "sip:bob@192.0.2.20:5060", headers, Vec::new())
        .expect("invite hydrates");
    InviteClientTransactionHandle { original_invite: invite }
}

// --- extract_non_structural_headers ---

#[test]
fn keeps_transparent_headers_and_drops_structural() {
    let a_leg = make_a_leg_invite();
    let kept = extract_non_structural_headers(&SipMessage::Request(a_leg));
    let mut names: Vec<String> = kept.iter().map(|h| h.name.to_ascii_lowercase()).collect();
    names.sort();
    assert_eq!(names, vec!["allow", "p-asserted-identity", "supported"]);
}

#[test]
fn preserves_order_among_non_structural() {
    let a_leg = make_a_leg_invite();
    let kept = extract_non_structural_headers(&SipMessage::Request(a_leg));
    assert_eq!(
        kept.iter().map(|h| h.name.clone()).collect::<Vec<_>>(),
        vec!["Allow", "Supported", "P-Asserted-Identity"]
    );
}

// --- generate_out_of_dialog_request ---

#[test]
fn builds_initial_invite_with_via_contact_maxforwards_content_length() {
    let req = generate_out_of_dialog_request(
        OutOfDialogMethod::Invite,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of("sip:bob@biloxi.example.com")),
            call_id: "call-bleg-1".to_string(),
            from: Some(from_hdr("sip:b2bua@10.0.0.1:5060", "b2bua-local")),
            to: Some(to_hdr("sip:bob@biloxi.example.com")),
            cseq: 1,
            via: Some(via_with_params()),
            contact: Some(contact_with_params()),
            body: sdp_body(),
            ..Default::default()
        },
    );
    assert_eq!(req.method(), "INVITE");
    assert_eq!(req.request_uri().text(), "sip:bob@biloxi.example.com");
    assert_eq!(
        first_value(req.headers(), "Via"),
        Some("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKtest00000000;cr=cref1;lg=a")
    );
    assert_eq!(
        first_value(req.headers(), "Contact"),
        Some("<sip:b2bua@10.0.0.1:5060;callRef=cref1;leg=a>")
    );
    assert_eq!(first_value(req.headers(), "Max-Forwards"), Some("70"));
    assert_eq!(first_value(req.headers(), "From"), Some("<sip:b2bua@10.0.0.1:5060>;tag=b2bua-local"));
    assert_eq!(first_value(req.headers(), "To"), Some("<sip:bob@biloxi.example.com>"));
    assert_eq!(first_value(req.headers(), "Call-ID"), Some("call-bleg-1"));
    assert_eq!(first_value(req.headers(), "CSeq"), Some("1 INVITE"));
    assert_eq!(first_value(req.headers(), "Content-Type"), Some("application/sdp"));
    assert_eq!(first_value(req.headers(), "Content-Length"), Some(sdp_body().len().to_string().as_str()));
    assert_eq!(&req.body()[..], &sdp_body());
}

#[test]
fn passes_extra_headers_through_verbatim() {
    let a_leg = make_a_leg_invite();
    let transparent = extract_non_structural_headers(&SipMessage::Request(a_leg.clone()));
    let req = generate_out_of_dialog_request(
        OutOfDialogMethod::Invite,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of("sip:bob@biloxi.example.com")),
            call_id: "call-bleg-1".to_string(),
            from: Some(from_hdr("sip:b2bua@10.0.0.1:5060", "b2bua-local")),
            to: Some(to_hdr("sip:bob@biloxi.example.com")),
            cseq: 1,
            via: Some(via()),
            contact: Some(contact()),
            extra_headers: transparent,
            body: a_leg.body().to_vec(),
            ..Default::default()
        },
    );
    assert_eq!(first_value(req.headers(), "Allow"), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
    assert_eq!(first_value(req.headers(), "Supported"), Some("replaces, 100rel"));
    assert_eq!(first_value(req.headers(), "P-Asserted-Identity"), Some("<sip:alice@atlanta.example.com>"));
}

#[test]
fn omits_content_type_when_body_empty() {
    let req = generate_out_of_dialog_request(
        OutOfDialogMethod::Options,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of("sip:bob@biloxi.example.com")),
            call_id: "cid".to_string(),
            from: Some(from_hdr("sip:b2bua@10.0.0.1:5060", "ft")),
            to: Some(to_hdr("sip:bob@biloxi.example.com")),
            cseq: 1,
            via: Some(via()),
            contact: Some(contact()),
            ..Default::default()
        },
    );
    assert_eq!(first_value(req.headers(), "Content-Type"), None);
    assert_eq!(first_value(req.headers(), "Content-Length"), Some("0"));
}

#[test]
fn honours_caller_provided_max_forwards() {
    let req = generate_out_of_dialog_request(
        OutOfDialogMethod::Invite,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of("sip:x@y")),
            call_id: "cid".to_string(),
            from: Some(from_hdr("sip:a@b", "ft")),
            to: Some(to_hdr("sip:x@y")),
            cseq: 1,
            via: Some(via()),
            contact: Some(contact()),
            max_forwards: Some(42),
            ..Default::default()
        },
    );
    assert_eq!(first_value(req.headers(), "Max-Forwards"), Some("42"));
}

#[test]
fn renders_a_from_display_name_and_its_tag_once() {
    let req = generate_out_of_dialog_request(
        OutOfDialogMethod::Invite,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of("sip:bob@biloxi.example.com")),
            call_id: "cid".to_string(),
            from: Some(
                header::From::from_uri(uri_of("sip:alice@atlanta.example.com"))
                    .with_display(SipStr::owned("Alice"))
                    .with_tag(SipStr::owned("alice-tag")),
            ),
            to: Some(to_hdr("sip:bob@biloxi.example.com")),
            cseq: 1,
            via: Some(via()),
            contact: Some(contact()),
            ..Default::default()
        },
    );
    assert_eq!(
        first_value(req.headers(), "From"),
        Some("\"Alice\" <sip:alice@atlanta.example.com>;tag=alice-tag")
    );
}

// --- generate_in_dialog_request ---

#[test]
fn bumps_cseq_uses_remote_target_swaps_tags() {
    let result = generate_in_dialog_request(
        InDialogMethod::Bye,
        &dialog(),
        &GenerateInDialogRequestOpts {
            via: Some(via_with_params()),
            contact: Some(contact_with_params()),
            ..Default::default()
        },
    );
    let request = &result.request;
    assert_eq!(request.method(), "BYE");
    assert_eq!(request.request_uri().text(), "sip:bob@192.0.2.20:5060");
    assert_eq!(first_value(request.headers(), "CSeq"), Some("101 BYE"));
    assert_eq!(result.dialog.local_cseq, 101);
    assert_eq!(first_value(request.headers(), "From"), Some("<sip:b2bua@10.0.0.1:5060>;tag=b2bua-local"));
    assert_eq!(first_value(request.headers(), "To"), Some("<sip:bob@192.0.2.20:5060>;tag=bob-remote"));
    assert_eq!(first_value(request.headers(), "Call-ID"), Some("call-bleg-1"));
}

/// Build a re-INVITE with the given capability declaration + extra headers and
/// return its `(Allow, Supported)` as they reach the wire.
fn reinvite_advert(
    capabilities: Option<CapabilitySet>,
    extra_headers: Vec<SipHeader>,
) -> (Option<String>, Option<String>) {
    let result = generate_in_dialog_request(
        InDialogMethod::Invite,
        &dialog(),
        &GenerateInDialogRequestOpts {
            via: Some(via()),
            contact: Some(contact()),
            capabilities,
            extra_headers,
            ..Default::default()
        },
    );
    let headers = result.request.headers().to_vec();
    (
        first_value(&headers, "Allow").map(str::to_string),
        first_value(&headers, "Supported").map(str::to_string),
    )
}

/// An undeclared re-INVITE advertises exactly the constants, byte for byte.
#[test]
fn reinvite_without_a_declared_capability_set_advertises_the_default() {
    let (allow, supported) = reinvite_advert(None, vec![]);
    assert_eq!(allow.as_deref(), Some(sip_message::generators::B2BUA_ALLOW));
    assert_eq!(supported.as_deref(), Some(sip_message::generators::B2BUA_SUPPORTED));
}

/// A declared set is what reaches the wire — no REFER, no 100rel.
#[test]
fn reinvite_advertises_the_declared_capability_set() {
    let caps = CapabilitySet::new(
        header::Allow::of(["INVITE", "ACK", "CANCEL", "BYE"]),
        header::Supported::of(["timer"]),
    );
    let (allow, supported) = reinvite_advert(Some(caps), vec![]);
    assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE"));
    assert_eq!(supported.as_deref(), Some("timer"));
}

/// An explicit `extra_headers` line is more specific than the declared set and
/// wins; the declaration still supplies the header the line does not name.
#[test]
fn an_explicit_header_line_beats_the_declared_capability_set() {
    let caps = CapabilitySet::new(
        header::Allow::of(["INVITE", "ACK", "CANCEL", "BYE"]),
        header::Supported::of(["timer"]),
    );
    let explicit =
        vec![SipHeader { name: "Allow".into(), value: "INVITE, ACK, BYE, NOTIFY".into() }];
    let (allow, supported) = reinvite_advert(Some(caps), explicit);
    assert_eq!(allow.as_deref(), Some("INVITE, ACK, BYE, NOTIFY"));
    assert_eq!(supported.as_deref(), Some("timer"));
}

/// The declaration rides INVITE only — a BYE/NOTIFY carries no advertisement.
#[test]
fn a_declared_capability_set_is_ignored_off_the_invite_path() {
    let result = generate_in_dialog_request(
        InDialogMethod::Bye,
        &dialog(),
        &GenerateInDialogRequestOpts {
            via: Some(via()),
            capabilities: Some(CapabilitySet::new(
                header::Allow::of(["INVITE"]),
                header::Supported::of(["timer"]),
            )),
            ..Default::default()
        },
    );
    assert_eq!(first_value(result.request.headers(), "Allow"), None);
    assert_eq!(first_value(result.request.headers(), "Supported"), None);
}

#[test]
fn emits_one_route_header_per_entry_in_order() {
    let d = StackDialog {
        route_set: vec![
            "<sip:proxy1.example.com;lr>".to_string(),
            "<sip:proxy2.example.com;lr>".to_string(),
        ],
        ..dialog()
    };
    let result = generate_in_dialog_request(
        InDialogMethod::Bye,
        &d,
        &GenerateInDialogRequestOpts { via: Some(via()), contact: Some(contact()), ..Default::default() },
    );
    assert_eq!(
        all_values(result.request.headers(), "Route"),
        vec!["<sip:proxy1.example.com;lr>", "<sip:proxy2.example.com;lr>"]
    );
    // Loose: Request-URI stays at the remote target.
    assert_eq!(result.request.request_uri().text(), "sip:bob@192.0.2.20:5060");
}

#[test]
fn strict_route_rewrites_request_uri_and_appends_remote_target() {
    // First route has NO `;lr` → strict routing (RFC 3261 §16.12): Request-URI
    // becomes the first route's URI, the remaining routes follow, and the
    // remote target is appended as the final Route.
    let d = StackDialog {
        route_set: vec![
            "<sip:strict1.example.com>".to_string(),
            "<sip:proxy2.example.com;lr>".to_string(),
        ],
        ..dialog()
    };
    let result = generate_in_dialog_request(
        InDialogMethod::Bye,
        &d,
        &GenerateInDialogRequestOpts { via: Some(via()), contact: Some(contact()), ..Default::default() },
    );
    assert_eq!(result.request.request_uri().text(), "sip:strict1.example.com", "R-URI = first (strict) route");
    assert_eq!(
        all_values(result.request.headers(), "Route"),
        vec!["<sip:proxy2.example.com;lr>", "<sip:bob@192.0.2.20:5060>"],
        "rest of route set then the remote target as the last Route"
    );
}

#[test]
fn loose_routing_is_a_uri_parameter_not_a_substring() {
    use sip_message::header::{HeaderValue, RouteEntry};
    let loose = |value: &str| {
        RouteEntry::parse(&SipStr::owned(value)).expect("readable Route").uri().is_loose_route()
    };
    assert!(loose("<sip:p;lr>"));
    assert!(loose("<sip:p;maddr=x;lr>"));
    assert!(loose("<sip:p:5060;lr;ob>"));
    assert!(!loose("<sip:p>"));
    assert!(!loose("<sip:lr-host.example.com>")); // not a `;lr` param
    assert!(!loose("<sip:p;lrx>")); // `;lr` must end the token
}

#[test]
fn adds_rack_on_prack() {
    let result = generate_in_dialog_request(
        InDialogMethod::Prack,
        &dialog(),
        &GenerateInDialogRequestOpts {
            via: Some(via()),
            contact: Some(contact()),
            rack: Some(RAck::new(1, 101, Method::Invite)),
            ..Default::default()
        },
    );
    assert_eq!(result.request.method(), "PRACK");
    assert_eq!(first_value(result.request.headers(), "RAck"), Some("1 101 INVITE"));
}

#[test]
fn adds_event_and_subscription_state_on_notify() {
    let body = b"SIP/2.0 180 Ringing\r\n".to_vec();
    let result = generate_in_dialog_request(
        InDialogMethod::Notify,
        &dialog(),
        &GenerateInDialogRequestOpts {
            via: Some(via()),
            contact: Some(contact()),
            event: Some(Event::parse(&SipStr::from_static("refer")).expect("readable Event")),
            subscription_state: Some(
                SubscriptionState::parse(&SipStr::from_static("active;expires=60"))
                    .expect("readable Subscription-State"),
            ),
            content_type: Some(
                MediaType::parse(&SipStr::from_static("message/sipfrag;version=2.0"))
                    .expect("readable Content-Type"),
            ),
            body,
            ..Default::default()
        },
    );
    assert_eq!(first_value(result.request.headers(), "Event"), Some("refer"));
    assert_eq!(first_value(result.request.headers(), "Subscription-State"), Some("active;expires=60"));
    assert_eq!(first_value(result.request.headers(), "Content-Type"), Some("message/sipfrag;version=2.0"));
}

#[test]
fn in_dialog_bye_omits_contact() {
    let result = generate_in_dialog_request(
        InDialogMethod::Bye,
        &dialog(),
        &GenerateInDialogRequestOpts { via: Some(via()), contact: Some(contact()), ..Default::default() },
    );
    assert_eq!(first_value(result.request.headers(), "Contact"), None);
}

// --- generate_ack_for_2xx ---

#[test]
fn reads_cseq_from_invite_handle_not_dialog() {
    let d = StackDialog { local_cseq: 999, ..dialog() };
    let ack = generate_ack_for_2xx(
        Some(&invite_handle()),
        &d,
        &GenerateAckFor2xxOpts {
            via: Some(via().with_branch("z9hG4bKackbranch")),
            ..Default::default()
        },
    );
    assert_eq!(ack.method(), "ACK");
    assert_eq!(first_value(ack.headers(), "CSeq"), Some("42 ACK"));
    assert!(first_value(ack.headers(), "Via").unwrap().contains("branch=z9hG4bKackbranch"));
}

#[test]
fn ack_for_2xx_empty_remote_tag_yields_tagless_to_no_panic() {
    // Fix B (ADR-0014): a dialog taken over mid-confirm by a reactive failover
    // (the relayed 2xx had not yet established the remote tag) has an EMPTY
    // remote_tag. The ACK must emit a tag-LESS To rather than a malformed
    // `;tag=` (which `hydrate_request` rejects → "Empty To tag parameter" panic).
    let d = StackDialog { remote_tag: String::new(), ..dialog() };
    let ack = generate_ack_for_2xx(
        Some(&invite_handle()),
        &d,
        &GenerateAckFor2xxOpts { via: Some(via()), ..Default::default() },
    );
    let to = first_value(ack.headers(), "To").expect("ACK has a To header");
    assert!(!to.contains("tag="), "empty remote_tag → tag-less To, got {to:?}");
    assert!(!to.contains(";tag="), "no malformed empty ;tag= that hydrate_request rejects");
}

#[test]
fn ack_request_uri_is_remote_target_routes_from_route_set() {
    let d = StackDialog {
        remote_target: "sip:bob-contact@192.0.2.99:5060".to_string(),
        route_set: vec!["<sip:proxy.example.com;lr>".to_string()],
        ..dialog()
    };
    let ack = generate_ack_for_2xx(
        Some(&invite_handle()),
        &d,
        &GenerateAckFor2xxOpts { via: Some(via()), ..Default::default() },
    );
    assert_eq!(ack.request_uri().text(), "sip:bob-contact@192.0.2.99:5060");
    assert_eq!(all_values(ack.headers(), "Route"), vec!["<sip:proxy.example.com;lr>"]);
}

#[test]
fn ack_carries_sdp_body() {
    let ack = generate_ack_for_2xx(
        Some(&invite_handle()),
        &dialog(),
        &GenerateAckFor2xxOpts { via: Some(via()), body: sdp_body(), ..Default::default() },
    );
    assert_eq!(first_value(ack.headers(), "Content-Type"), Some("application/sdp"));
    assert_eq!(first_value(ack.headers(), "Content-Length"), Some(sdp_body().len().to_string().as_str()));
    assert_eq!(&ack.body()[..], &sdp_body());
}

// --- generate_cancel ---

#[test]
fn cancel_reuses_invite_topmost_via_verbatim() {
    let cancel = generate_cancel(&invite_handle());
    assert_eq!(cancel.method(), "CANCEL");
    assert_eq!(
        first_value(cancel.headers(), "Via"),
        Some("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKinvite123;cr=cref1;lg=b-1")
    );
}

#[test]
fn cancel_mirrors_request_uri_callid_from_to_cseq() {
    let cancel = generate_cancel(&invite_handle());
    assert_eq!(cancel.request_uri().text(), "sip:bob@192.0.2.20:5060");
    assert_eq!(first_value(cancel.headers(), "Call-ID"), Some("call-bleg-1"));
    assert_eq!(first_value(cancel.headers(), "From"), Some("<sip:b2bua@10.0.0.1:5060>;tag=b2bua-local"));
    assert_eq!(first_value(cancel.headers(), "To"), Some("<sip:bob@192.0.2.20:5060>"));
    assert_eq!(first_value(cancel.headers(), "CSeq"), Some("42 CANCEL"));
    assert_eq!(first_value(cancel.headers(), "Content-Length"), Some("0"));
}

#[test]
fn cancel_and_non_2xx_ack_echo_the_invite_request_uri_octet_for_octet() {
    // RFC 3261 §9.1 and §17.1.1.3: both requests carry the INVITE's
    // Request-URI, so a URI whose spelling a value model would tidy — an
    // uppercase scheme, an escaped header with no value — leaves as the octets
    // the INVITE was sent with.
    let r_uri = "SIP:bob@192.0.2.20:5060;transport=TCP?X-Trace";
    let headers = vec![
        hdr("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKinvite123"),
        hdr("Max-Forwards", "70"),
        hdr("From", "<sip:b2bua@10.0.0.1:5060>;tag=b2bua-local"),
        hdr("To", "<sip:bob@192.0.2.20:5060>"),
        hdr("Call-ID", "call-bleg-1"),
        hdr("CSeq", "42 INVITE"),
        hdr("Content-Length", "0"),
    ];
    let invite = hydrate_request("INVITE", r_uri, headers, Vec::new()).expect("invite hydrates");
    let handle = InviteClientTransactionHandle { original_invite: invite };

    assert_eq!(generate_cancel(&handle).request_uri().text(), r_uri);

    let final487 = generate_response(
        &handle.original_invite,
        487,
        "Request Terminated",
        &GenerateResponseOpts { to_tag: Some("uas-tag".to_string()), ..Default::default() },
    );
    assert_eq!(
        generate_ack_for_non_2xx(&handle.original_invite, &final487).request_uri().text(),
        r_uri
    );
}

#[test]
fn cancel_echoes_the_invite_route_set_verbatim() {
    // RFC 3261 §9.1: a CANCEL takes the same path as the INVITE it cancels, so
    // its Route header fields MUST equal the INVITE's. When the b-leg INVITE
    // egressed through the front proxy it carried a preloaded outbound-proxy
    // Route; the CANCEL must reproduce it (else it bypasses the proxy and never
    // reaches the pending server txn). Two Route values pin ordering too.
    let headers = vec![
        hdr("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKinvite123;cr=cref1;lg=b-1"),
        hdr("Max-Forwards", "70"),
        hdr("Route", "<sip:proxy.example:5060;lr>"),
        hdr("Route", "<sip:edge.example:5060;lr>"),
        hdr("From", "<sip:b2bua@10.0.0.1:5060>;tag=b2bua-local"),
        hdr("To", "<sip:bob@192.0.2.20:5060>"),
        hdr("Call-ID", "call-bleg-1"),
        hdr("CSeq", "42 INVITE"),
        hdr("Content-Length", "0"),
    ];
    let invite = hydrate_request("INVITE", "sip:bob@192.0.2.20:5060", headers, Vec::new())
        .expect("invite hydrates");
    let cancel = generate_cancel(&InviteClientTransactionHandle { original_invite: invite });

    let cancel_routes: Vec<String> = cancel
        .headers()
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("Route"))
        .map(|h| h.value.to_string())
        .collect();
    assert_eq!(
        cancel_routes,
        vec![
            "<sip:proxy.example:5060;lr>".to_string(),
            "<sip:edge.example:5060;lr>".to_string(),
        ],
        "CANCEL must echo the INVITE Route set in order (RFC 3261 §9.1)"
    );
    // Via branch (the transaction-correlation key) still matches the INVITE.
    assert_eq!(
        first_value(cancel.headers(), "Via"),
        Some("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKinvite123;cr=cref1;lg=b-1")
    );
}

// --- generate_response ---

#[test]
fn echoes_via_from_to_callid_cseq() {
    let req = make_a_leg_invite();
    let resp = generate_response(&req, 100, "Trying", &GenerateResponseOpts::default());
    assert_eq!(first_value(resp.headers(), "Via"), first_value(req.headers(), "Via"));
    assert_eq!(first_value(resp.headers(), "From"), first_value(req.headers(), "From"));
    assert_eq!(first_value(resp.headers(), "To"), first_value(req.headers(), "To"));
    assert_eq!(first_value(resp.headers(), "Call-ID"), first_value(req.headers(), "Call-ID"));
    assert_eq!(first_value(resp.headers(), "CSeq"), first_value(req.headers(), "CSeq"));
}

#[test]
fn adds_to_tag_when_status_gt_100_and_request_lacks_one() {
    let req = make_a_leg_invite();
    let resp = generate_response(
        &req,
        180,
        "Ringing",
        &GenerateResponseOpts { to_tag: Some("b2bua-uas-tag".to_string()), ..Default::default() },
    );
    assert_eq!(first_value(resp.headers(), "To"), Some("<sip:bob@biloxi.example.com>;tag=b2bua-uas-tag"));
}

#[test]
fn mints_fallback_to_tag_when_caller_supplies_none_on_non100() {
    // Regression: a >100 response to a tag-less request with NO opts.to_tag must
    // NOT panic with "missing mandatory To-tag" (that crashes the worker handler,
    // leaks the dialog, and OOMs under load). It mints a deterministic fallback
    // so a well-formed response is always emitted.
    let req = make_a_leg_invite();
    let resp = generate_response(&req, 200, "OK", &GenerateResponseOpts::default());
    let to = first_value(resp.headers(), "To").expect("To header");
    assert!(to.contains(";tag=b2bua-fb-"), "expected fallback To-tag, got {to:?}");
    // Deterministic per Call-ID: a retransmit re-derives the same tag.
    let resp2 = generate_response(&req, 200, "OK", &GenerateResponseOpts::default());
    assert_eq!(first_value(resp2.headers(), "To"), Some(to));
}

#[test]
fn does_not_add_tag_on_100_trying() {
    let req = make_a_leg_invite();
    let resp = generate_response(
        &req,
        100,
        "Trying",
        &GenerateResponseOpts { to_tag: Some("should-be-ignored".to_string()), ..Default::default() },
    );
    assert_eq!(first_value(resp.headers(), "To"), Some("<sip:bob@biloxi.example.com>"));
}

#[test]
fn preserves_already_present_to_tag() {
    let req = make_a_leg_invite()
        .thaw()
        .set(sip_message::header::To::parse(&SipStr::owned(
            "<sip:bob@biloxi.example.com>;tag=existing",
        ))
        .expect("readable To"))
        .freeze()
        .expect("still complete");
    let resp = generate_response(
        &req,
        200,
        "OK",
        &GenerateResponseOpts {
            to_tag: Some("different".to_string()),
            contact: Some(contact()),
            ..Default::default()
        },
    );
    assert_eq!(first_value(resp.headers(), "To"), Some("<sip:bob@biloxi.example.com>;tag=existing"));
}

#[test]
fn emits_contact_when_provided() {
    let req = make_a_leg_invite();
    let resp = generate_response(
        &req,
        200,
        "OK",
        &GenerateResponseOpts {
            to_tag: Some("b2bua-uas-tag".to_string()),
            contact: Some(contact_with_params()),
            ..Default::default()
        },
    );
    assert_eq!(first_value(resp.headers(), "Contact"), Some("<sip:b2bua@10.0.0.1:5060;callRef=cref1;leg=a>"));
}

#[test]
fn omits_contact_when_not_provided() {
    let req = make_a_leg_invite();
    let resp = generate_response(
        &req,
        486,
        "Busy Here",
        &GenerateResponseOpts { to_tag: Some("b2bua-uas-tag".to_string()), ..Default::default() },
    );
    assert_eq!(first_value(resp.headers(), "Contact"), None);
}

// --- generate_ack_for_non_2xx ---

#[test]
fn ack_non_2xx_reuses_invite_via_and_copies_response_from_to() {
    let handle = invite_handle();
    let final487 = generate_response(
        &handle.original_invite,
        487,
        "Request Terminated",
        &GenerateResponseOpts { to_tag: Some("uas-tag".to_string()), ..Default::default() },
    );
    let ack = generate_ack_for_non_2xx(&handle.original_invite, &final487);
    assert_eq!(ack.method(), "ACK");
    assert_eq!(ack.request_uri().text(), "sip:bob@192.0.2.20:5060");
    assert_eq!(first_value(ack.headers(), "Via"), first_value(handle.original_invite.headers(), "Via"));
    assert_eq!(first_value(ack.headers(), "From"), first_value(final487.headers(), "From"));
    assert_eq!(first_value(ack.headers(), "To"), first_value(final487.headers(), "To"));
    assert_eq!(first_value(ack.headers(), "Call-ID"), Some("call-bleg-1"));
    assert_eq!(first_value(ack.headers(), "CSeq"), Some("42 ACK"));
}

#[test]
fn ack_non_2xx_echoes_the_invite_route_set_verbatim() {
    // RFC 3261 §17.1.1.3: a non-2xx ACK belongs to the INVITE transaction and is
    // routed the same way — its Route header fields MUST equal the INVITE's.
    // Load-bearing in the via-LB topology: the ACK for a 486 on a proxy-egressed
    // b-leg must retain the preloaded outbound-proxy Route so it reaches the same
    // hop the INVITE (and its pending server txn) traversed.
    let headers = vec![
        hdr("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKinvite123;cr=cref1;lg=b-1"),
        hdr("Max-Forwards", "70"),
        hdr("Route", "<sip:proxy.example:5060;lr>"),
        hdr("From", "<sip:b2bua@10.0.0.1:5060>;tag=b2bua-local"),
        hdr("To", "<sip:bob@192.0.2.20:5060>"),
        hdr("Call-ID", "call-bleg-1"),
        hdr("CSeq", "42 INVITE"),
        hdr("Content-Length", "0"),
    ];
    let invite = hydrate_request("INVITE", "sip:bob@192.0.2.20:5060", headers, Vec::new())
        .expect("invite hydrates");
    let final486 = generate_response(
        &invite,
        486,
        "Busy Here",
        &GenerateResponseOpts { to_tag: Some("uas-tag".to_string()), ..Default::default() },
    );
    let ack = generate_ack_for_non_2xx(&invite, &final486);
    let ack_routes: Vec<String> = ack
        .headers()
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("Route"))
        .map(|h| h.value.to_string())
        .collect();
    assert_eq!(
        ack_routes,
        vec!["<sip:proxy.example:5060;lr>".to_string()],
        "non-2xx ACK must echo the INVITE Route set (RFC 3261 §17.1.1.3)"
    );
}
