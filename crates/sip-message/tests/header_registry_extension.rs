//! Third-party typed-header extension. Port of
//! `tests/sip/header-registry-extension.test.ts`.
//!
//! ADR-0003 §E replaces the TS runtime registry (`defaultRegistry.register`) +
//! declaration-merging with the open, compile-time [`TypedHeader`] trait:
//! integrators implement it in their own crate and call `msg.typed::<H>()`.
//! Differences from the TS contract, with justification:
//!   - **Memoization** (TS asserted parse runs once per message): not ported —
//!     `typed()` re-parses on each call by design (types.rs documents this;
//!     Rust parsing is cheap, a type-erased per-message cache fights the borrow
//!     checker). The "parse called exactly once" assertions don't apply.
//!   - **Collision guard** (TS: re-registering `from` throws at runtime): in
//!     Rust the built-ins are concrete fields, NOT `TypedHeader` impls, so a
//!     third party *cannot* shadow them — the guard is structural / compile-time,
//!     leaving nothing to assert at runtime.

use sip_message::{CustomParser, SipMessage, SipParser, SipParseError, TypedHeader};

#[derive(Debug, PartialEq, Eq)]
struct RoutingHint {
    hop: String,
    priority: i64,
}

/// A typed view over every `X-Routing-Hint` header on the message.
#[derive(Debug, PartialEq, Eq)]
struct RoutingHints(Vec<RoutingHint>);

impl TypedHeader for RoutingHints {
    const NAME: &'static str = "x-routing-hint";

    fn parse(raw_values: &[&str]) -> Result<Self, SipParseError> {
        let hints = raw_values
            .iter()
            .map(|r| {
                let (hop, prio) = r.split_once(':').unwrap_or((r, "0"));
                RoutingHint { hop: hop.to_string(), priority: prio.trim().parse().unwrap_or(0) }
            })
            .collect();
        Ok(RoutingHints(hints))
    }
}

fn invite_with(extra: &str) -> Vec<u8> {
    format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-ext\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: ext-test\r\n\
CSeq: 1 INVITE\r\n\
{extra}\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new().parse(raw).expect("parse")
}

#[test]
fn registered_typed_header_returns_typed_value() {
    let msg = parse(&invite_with("X-Routing-Hint: edge-a:10\r\nX-Routing-Hint: edge-b:20\r\n"));
    let hints = msg.typed::<RoutingHints>().expect("typed parse");
    assert_eq!(
        hints.0,
        vec![
            RoutingHint { hop: "edge-a".to_string(), priority: 10 },
            RoutingHint { hop: "edge-b".to_string(), priority: 20 },
        ]
    );
}

#[test]
fn typed_access_is_per_message() {
    // No shared/global state: two messages yield their own values.
    let msg_a = parse(&invite_with("X-Routing-Hint: A\r\n"));
    let msg_b = parse(&invite_with("X-Routing-Hint: B\r\n"));
    assert_eq!(msg_a.typed::<RoutingHints>().unwrap().0, vec![RoutingHint { hop: "A".to_string(), priority: 0 }]);
    assert_eq!(msg_b.typed::<RoutingHints>().unwrap().0, vec![RoutingHint { hop: "B".to_string(), priority: 0 }]);
}

#[test]
fn unknown_header_falls_through_to_raw_strings() {
    let msg = parse(&invite_with("X-Acme-Trace: hop-1\r\nX-Acme-Trace: hop-2\r\n"));
    // No TypedHeader impl for x-acme-trace → the raw Vec<&str> escape hatch.
    assert_eq!(msg.get_header("x-acme-trace"), vec!["hop-1", "hop-2"]);
}
