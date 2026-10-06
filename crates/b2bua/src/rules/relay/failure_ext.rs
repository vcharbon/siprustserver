//! The relayed-failure-headers `Call.ext` slot: the relayable header image of
//! the failure round trip in flight (the `/call/failure` consult), folded only
//! into the a-facing final that answers that consult (ADR-0017 X2). A reserved
//! core slot rides replicated call state but never reaches a decision backend
//! (ADR-0016).

use sip_message::{SipHeader as MsgHeader, SipStr};

use sip_message::generators::{RelayScope, SourceBody};

use crate::config::B2buaConfig;

use super::passthrough::relay_response_passthrough_headers;

/// `Call.ext` slot carrying the relayable header image of the failure round
/// trip IN FLIGHT — the `/call/failure` consult the caller is still waiting on.
/// **Every** consult restates it, null when that failure produced no peer
/// final (a no-answer or transaction timeout), so a superseded attempt's
/// headers can never outlive their own failure; an answer (`confirm-dialog`)
/// clears it, so an established call replicates none of it. Only the final
/// answering that consult folds it, under the decision's `header_updates`
/// (ADR-0017 X2).
pub use b2bua_sdk::failure_image::RELAYED_FAILURE_HEADERS_EXT;

/// Is this `Call.ext` key the CORE's own slot rather than a service id? A
/// reserved key rides the replicated call state but is never a service slice,
/// so it never reaches a decision backend (ADR-0016).
pub fn is_core_reserved_ext(key: &str) -> bool {
    key == RELAYED_FAILURE_HEADERS_EXT
}

/// The one-entry `Call.ext` merge every `/call/failure` consult states: the
/// failing final's relayable image for the [`RELAYED_FAILURE_HEADERS_EXT`]
/// slot — a JSON array of `[name, value]` pairs, wire order and repeats kept,
/// body dropped ([`relay_response_passthrough_headers`], since the minted final
/// never carries the source's body; under `config`'s privacy-service role) —
/// or JSON null, which CLEARS the slot, when the failure has no peer final to
/// state. The relay policy is read where the final answering the consult is
/// minted, for that final's status (`actions::respond`). The image keeps the
/// peer's clock stamps, its `Timestamp` as the echo of `answered`'s (RFC 3261
/// §8.2.6.1), which the relay treatment carries onward; a decision-authored
/// final leaves them behind.
pub fn failure_headers_ext(
    resp: Option<&sip_message::SipResponse>,
    answered: &call::ALegInviteSnapshot,
    config: &B2buaConfig,
) -> call::ExtMap {
    let Some(resp) = resp else {
        return b2bua_sdk::failure_image::no_failure_image();
    };
    let scope = RelayScope::response_carrying(SourceBody::Dropped)
        .with_identity(config.identity_privacy())
        .stamped(b2bua_sdk::relayed_final::invite_timestamp(answered));
    let pairs: Vec<serde_json::Value> = relay_response_passthrough_headers(resp, scope)
        .iter()
        .map(|h| serde_json::json!([h.name.as_str(), h.value.as_str()]))
        .collect();
    let mut ext = call::ExtMap::new();
    ext.insert(RELAYED_FAILURE_HEADERS_EXT.to_string(), serde_json::Value::Array(pairs));
    ext
}

/// Decode the [`RELAYED_FAILURE_HEADERS_EXT`] slot back into headers: `None`
/// when the failure round trip in flight produced no peer final (a no-answer
/// or transaction timeout) or no round trip is in flight; the peer final's
/// relayable lines, possibly none, otherwise.
pub fn relayed_failure_headers(ext: Option<&call::ExtMap>) -> Option<Vec<MsgHeader>> {
    let pairs = ext.and_then(|m| m.get(RELAYED_FAILURE_HEADERS_EXT))?.as_array()?;
    Some(
        pairs
            .iter()
            .filter_map(|p| {
                let name = p.get(0)?.as_str()?;
                let value = p.get(1)?.as_str()?;
                Some(MsgHeader { name: SipStr::owned(name), value: SipStr::owned(value) })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::generators::{MessageClass, RelayPolicy};
    use sip_message::method::Method;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    /// The caller's INVITE, stating `lines`.
    fn invite(lines: &[(&str, &str)]) -> call::ALegInviteSnapshot {
        call::ALegInviteSnapshot {
            uri: "sip:bob@b.example".into(),
            headers: lines
                .iter()
                .map(|(n, v)| call::SipHeader { name: (*n).into(), value: (*v).into() })
                .collect(),
            body: Vec::new(),
            cseq: 1,
        }
    }

    fn busy() -> sip_message::SipResponse {
        let raw = "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK-b\r\n\
From: <sip:alice@a.example>;tag=a1\r\n\
To: <sip:bob@b.example>;tag=b1\r\n\
Call-ID: b-leg\r\n\
CSeq: 1 INVITE\r\n\
X-Vendor-Thing: opaque-42\r\n\
Content-Length: 0\r\n\r\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Response(r) => r,
            _ => panic!("expected a response"),
        }
    }

    /// The image is stored whole: an entry naming one of its lines for the
    /// callee's status removes nothing here, because the policy is read for
    /// the final that answers the consult, whose status the decision picks.
    #[test]
    fn the_image_is_stored_before_the_policy_is_read() {
        let config = B2buaConfig {
            relay_policy: RelayPolicy::transparent().dropping(
                "X-Vendor-Thing",
                MessageClass::Response { class: 4, method: Method::Invite },
                None,
            ),
            ..B2buaConfig::default()
        };
        let image = relayed_failure_headers(Some(&failure_headers_ext(
            Some(&busy()),
            &invite(&[]),
            &config,
        )))
        .expect("a peer final");
        let names: Vec<&str> = image.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["X-Vendor-Thing"]);
    }

    /// The callee's echo answers what the callee received; the image the
    /// caller's final carries echoes her own INVITE's value (RFC 3261
    /// §8.2.6.1), and none where her INVITE stated none.
    #[test]
    fn the_image_echoes_the_callers_timestamp() {
        let stamped = busy()
            .thaw()
            .push_raw(sip_message::HeaderName::Timestamp, "77.7 0.1")
            .freeze()
            .expect("the final");
        let echo = |answered| -> Vec<String> {
            let ext = failure_headers_ext(Some(&stamped), &answered, &B2buaConfig::default());
            relayed_failure_headers(Some(&ext))
                .expect("a peer final")
                .iter()
                .filter(|h| h.name.eq_ignore_ascii_case("Timestamp"))
                .map(|h| h.value.to_string())
                .collect()
        };
        assert_eq!(echo(invite(&[("Timestamp", "5.25")])), ["5.25"]);
        assert!(echo(invite(&[])).is_empty());
    }

    /// A peer final carrying no relayable line is still a peer final: an
    /// image with no lines, not no image.
    #[test]
    fn a_bare_peer_final_decodes_to_an_empty_image() {
        let raw = "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK-b\r\n\
From: <sip:alice@a.example>;tag=a1\r\n\
To: <sip:bob@b.example>;tag=b1\r\n\
Call-ID: b-leg\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let SipMessage::Response(bare) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
            panic!("expected a response")
        };
        let ext = failure_headers_ext(Some(&bare), &invite(&[]), &B2buaConfig::default());
        assert_eq!(relayed_failure_headers(Some(&ext)), Some(Vec::new()));
    }

    /// A round trip with no peer final, and none in flight, decode to no
    /// image at all: nothing is there to restate.
    #[test]
    fn no_peer_final_decodes_to_no_image() {
        let config = B2buaConfig::default();
        assert_eq!(
            relayed_failure_headers(Some(&failure_headers_ext(None, &invite(&[]), &config))),
            None
        );
        assert_eq!(relayed_failure_headers(None), None);
    }
}
