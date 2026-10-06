//! The peer final an a-facing answer delivers, reduced to the header lines of
//! it that ride onto the answer the back-to-back UA mints (RFC 3261 §16.6).

use call::ALegInviteSnapshot;
use sip_message::generators::{self, RelayDirection, RelayScope, RelaySituation, SourceBody};

use crate::B2buaConfig;
use sip_message::{SipHeader, SipResponse};

/// The `Timestamp` an answer to the originator's INVITE echoes (RFC 3261
/// §8.2.6.1): the value her INVITE stated, `None` where it stated none.
pub fn invite_timestamp(answered: &ALegInviteSnapshot) -> Option<&str> {
    generators::stated_timestamp(
        answered.headers.iter().map(|h| (h.name.as_str(), h.value.as_str())),
    )
}

/// The lines of a peer final that ride onto the a-facing answer delivering it.
/// Built only from the response itself, by the one relay rule every response
/// exit shares ([`generators::relayable_headers`]), so an action can never state
/// a relay of what it did not receive. [`Self::none`] is an answer the B2BUA
/// gives on its own behalf: no peer final, nothing rides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayedFinal {
    headers: Vec<SipHeader>,
}

impl RelayedFinal {
    /// An answer minted on the B2BUA's own behalf — nothing rides.
    pub const fn none() -> Self {
        Self { headers: Vec::new() }
    }

    /// What of `delivered` rides onto the answer of `status` to `answered`,
    /// `body` stating what the answer carries where the final had a body: the
    /// same octets, a staged replacement, or none. The answer travels toward
    /// the caller under `config`'s privacy-service role and relay policy, read
    /// for `status` ([`B2buaConfig::relay_scope`]); a `Timestamp` the final
    /// stated is the echo of `answered`'s ([`invite_timestamp`]).
    pub fn of(
        delivered: &SipResponse,
        answered: &ALegInviteSnapshot,
        status: u16,
        body: SourceBody,
        config: &B2buaConfig,
    ) -> Self {
        let situation = RelaySituation::response(
            status,
            delivered.cseq().method(),
            RelayDirection::TowardCaller,
        );
        let scope = config
            .relay_scope(RelayScope::response_carrying(body), situation)
            .stamped(invite_timestamp(answered));
        Self { headers: generators::relayable_headers(delivered.headers(), scope) }
    }

    /// The lines that ride, in the final's wire order, repeats kept.
    pub fn headers(&self) -> &[SipHeader] {
        &self.headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::header::HeaderName;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    /// A callee 200 stating privacy, an identity line of its own, a vendor
    /// header, its capability advert and session interval, plus lines the
    /// stack owns.
    fn callee_200() -> SipResponse {
        let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK-b\r\n\
Record-Route: <sip:proxy.bob.example;lr>\r\n\
From: <sip:alice@a.example>;tag=a1\r\n\
To: <sip:bob@b.example>;tag=b1\r\n\
Call-ID: b-leg\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>\r\n\
Privacy: none\r\n\
X-Vendor-Id: 100000042\r\n\
X-Vendor-Thing: opaque-42\r\n\
Allow: INVITE, ACK, BYE\r\n\
Session-Expires: 1800;refresher=uas\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 4\r\n\r\nv=0\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Response(r) => r,
            _ => panic!("expected a response"),
        }
    }

    /// The caller's INVITE the answer answers, stating `Timestamp: 12.5`.
    fn answered() -> ALegInviteSnapshot {
        ALegInviteSnapshot {
            uri: "sip:bob@b.example".into(),
            headers: vec![call::SipHeader { name: "Timestamp".into(), value: "12.5".into() }],
            body: Vec::new(),
            cseq: 1,
        }
    }

    fn names(relayed: &RelayedFinal) -> Vec<String> {
        relayed.headers().iter().map(|h| h.name.to_ascii_lowercase()).collect()
    }

    /// The end-to-end lines ride, the session-timer negotiation among them
    /// (RFC 4028); the structural set and the Contact do not.
    #[test]
    fn a_delivered_final_contributes_exactly_its_relayable_lines() {
        let carried = names(&RelayedFinal::of(
            &callee_200(),
            &answered(),
            200,
            SourceBody::Verbatim,
            &B2buaConfig::default(),
        ));
        for name in ["privacy", "x-vendor-id", "x-vendor-thing", "allow", "session-expires"] {
            assert!(carried.contains(&name.to_string()), "{name} must ride: {carried:?}");
        }
        let stays =
            ["via", "record-route", "from", "to", "call-id", "cseq", "contact", "content-type"];
        for name in stays {
            assert!(!carried.contains(&name.to_string()), "{name} must not ride: {carried:?}");
        }
    }

    /// The deployment's relay policy reads the answer as a 2xx to an INVITE
    /// travelling toward the caller.
    #[test]
    fn the_relay_policy_reads_the_answer_toward_the_caller() {
        use sip_message::generators::{MessageClass, RelayPolicy};
        use sip_message::method::Method;
        let answer = MessageClass::Response { class: 2, method: Method::Invite };
        let config = |toward| B2buaConfig {
            relay_policy: RelayPolicy::transparent().dropping(
                "X-Vendor-Id",
                answer.clone(),
                toward,
            ),
            ..B2buaConfig::default()
        };
        let carried = |config| {
            names(&RelayedFinal::of(&callee_200(), &answered(), 200, SourceBody::Verbatim, &config))
        };
        assert!(
            !carried(config(Some(RelayDirection::TowardCaller))).contains(&"x-vendor-id".into())
        );
        assert!(carried(config(Some(RelayDirection::TowardCallee))).contains(&"x-vendor-id".into()));
    }

    /// The policy reads the status of the answer being minted, not the
    /// status of the final it delivers.
    #[test]
    fn the_relay_policy_reads_the_minted_status() {
        use sip_message::generators::{MessageClass, RelayPolicy};
        use sip_message::method::Method;
        let config = B2buaConfig {
            relay_policy: RelayPolicy::transparent().dropping(
                "X-Vendor-Id",
                MessageClass::Response { class: 6, method: Method::Invite },
                None,
            ),
            ..B2buaConfig::default()
        };
        let carried = |status| {
            names(&RelayedFinal::of(
                &callee_200(),
                &answered(),
                status,
                SourceBody::Verbatim,
                &config,
            ))
        };
        assert!(!carried(603).contains(&"x-vendor-id".into()), "an answer of 603 is a 6xx");
        assert!(carried(200).contains(&"x-vendor-id".into()), "an answer of 200 is not");
    }

    /// The callee's echo answers the request the callee received; the caller
    /// is answered with the echo of her own INVITE's value (RFC 3261 §8.2.6.1),
    /// and with none where her INVITE stated none.
    #[test]
    fn a_timestamp_echoes_the_callers_invite() {
        let stamped = callee_200()
            .thaw()
            .push_raw(HeaderName::Timestamp, "99.1 0.2")
            .freeze()
            .expect("the final");
        let echo = |answered: &ALegInviteSnapshot| -> Vec<String> {
            RelayedFinal::of(&stamped, answered, 200, SourceBody::Verbatim, &B2buaConfig::default())
                .headers()
                .iter()
                .filter(|h| HeaderName::Timestamp.matches(&h.name))
                .map(|h| h.value.to_string())
                .collect()
        };
        assert_eq!(echo(&answered()), ["12.5"]);
        let silent = ALegInviteSnapshot { headers: Vec::new(), ..answered() };
        assert!(echo(&silent).is_empty(), "no echo of a value she never stated");
        let unstamped = RelayedFinal::of(
            &callee_200(),
            &answered(),
            200,
            SourceBody::Verbatim,
            &B2buaConfig::default(),
        );
        assert!(
            !names(&unstamped).contains(&"timestamp".to_string()),
            "a final stating none states none"
        );
    }

    /// An answer on the B2BUA's own behalf relays nothing.
    #[test]
    fn none_relays_nothing() {
        assert!(RelayedFinal::none().headers().is_empty());
        assert_eq!(RelayedFinal::none(), RelayedFinal::default());
    }
}
