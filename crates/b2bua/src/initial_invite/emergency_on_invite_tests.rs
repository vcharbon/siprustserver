//! Pins that [`build_initial_call`] stamps `Call.emergency` from the
//! inbound INVITE: `Some(true)` for an emergency Resource-Priority,
//! *absence* (`None`) otherwise — never `Some(false)`. The field feeds the
//! `;emerg=1`/`;em=1` stack-identity markers stamped on the call's outbound
//! hops.
//!
//! These assert the *wiring* (helper → field + the None coercion), not the
//! emergency-classification contract itself — that lives in
//! `sip_message::emergency`. Pure builder, no clock.

use super::build_initial_call;
use crate::config::B2buaConfig;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use std::net::SocketAddr;

fn config_for(ordinal: &str) -> B2buaConfig {
    B2buaConfig { self_ordinal: ordinal.into(), ..Default::default() }
}

fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

/// Parse a minimal INVITE; `rph` (when `Some`) is the `Resource-Priority`
/// header VALUE (header name canonical). `None` omits the header entirely.
fn invite_with_rph(rph: Option<&str>) -> SipRequest {
    let rph_line = match rph {
        Some(v) => format!("Resource-Priority: {v}\r\n"),
        None => String::new(),
    };
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-emerg-iih\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: emerg-iih@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             {rph_line}\
             Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

#[test]
fn emergency_invite_sets_emergency_true() {
    // Each canonical emergency RPH token flags the new a-leg Call as emergency.
    for tok in ["esnet.0", "wps.0", "q735.0"] {
        let invite = invite_with_rph(Some(tok));
        let call = build_initial_call(
            &invite,
            src(),
            &config_for("w0"),
            &mut sip_ids::TurnIds::fixed(1),
            0,
        );
        assert_eq!(
            call.emergency,
            Some(true),
            "RPH {tok} must stamp emergency = Some(true) on the a-leg Call"
        );
    }
}

#[test]
fn non_emergency_invite_leaves_emergency_unset() {
    // No Resource-Priority at all → field stays absent (`None`), NOT Some(false).
    let call = build_initial_call(
        &invite_with_rph(None),
        src(),
        &config_for("w0"),
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    assert_eq!(call.emergency, None, "no RPH → emergency stays None");

    // A well-formed but non-emergency RPH namespace.value also stays None.
    let call = build_initial_call(
        &invite_with_rph(Some("dsn.flash")),
        src(),
        &config_for("w0"),
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    assert_eq!(call.emergency, None, "non-emergency RPH → emergency stays None");
}

#[test]
fn emergency_is_derived_through_the_real_helper() {
    // Proves the field is wired to `is_emergency_request` and not a naive
    // header-presence check: r-values compare case-insensitively (RFC 4412)…
    let call = build_initial_call(
        &invite_with_rph(Some("ESNET.0")),
        src(),
        &config_for("w0"),
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    assert_eq!(call.emergency, Some(true), "r-value casing must not gate emergency");

    // …an emergency r-value among multiple namespaces flags…
    let call = build_initial_call(
        &invite_with_rph(Some("dsn.flash, q735.0")),
        src(),
        &config_for("w0"),
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    assert_eq!(call.emergency, Some(true), "emergency r-value in a list flags emergency");

    // …and an r-value merely embedding a token does not.
    let call = build_initial_call(
        &invite_with_rph(Some("esnet.01")),
        src(),
        &config_for("w0"),
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    assert_eq!(call.emergency, None, "embedded token is not an emergency r-value");
}
