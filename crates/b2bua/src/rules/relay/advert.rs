//! The capability advertisement (`Allow`/`Supported`/`Accept`) stamp toward
//! the originator face. The originated-leg half is stamped at
//! [`super::originate::build_b_leg`]; the set itself is resolved by
//! [`crate::rules::capabilities`].

use sip_message::draft::Entry;
use sip_message::generators::CapabilitySet;
use sip_message::header::HeaderName;
use sip_message::{SipHeader as MsgHeader, SipStr};

/// Ensure an a-facing INVITE 2xx header set carries at most ONE `Allow`, ONE
/// `Supported` and ONE `Accept` (RFC 3261 §13.2.1/§20.37/§20.1): a header the
/// firing rule stamped (`rule_stamped`) is kept verbatim and de-duplicated;
/// every other passed-through line collapses into `capabilities`' resolved
/// value, or into no line where the set states none. `Require`/`RSeq` are
/// untouched.
pub fn stamp_a_facing_invite_advert(
    headers: &mut Vec<MsgHeader>,
    rule_stamped: &[Entry],
    capabilities: &CapabilitySet,
) {
    for (name, value) in [
        (HeaderName::Allow, capabilities.allow_text()),
        (HeaderName::Supported, capabilities.supported_text()),
        (HeaderName::Accept, capabilities.accept_text()),
    ] {
        if rule_stamped.iter().any(|e| e.is(&name)) {
            // The rule owns this value; just collapse any duplicate to one.
            let mut seen = false;
            headers.retain(|h| {
                if name.matches(&h.name) {
                    let keep = !seen;
                    seen = true;
                    keep
                } else {
                    true
                }
            });
            continue;
        }
        // Replace any passed-through value with this face's set, exactly once;
        // a half the set does not state carries no line.
        headers.retain(|h| !name.matches(&h.name));
        if let Some(value) = value {
            headers.push(MsgHeader {
                name: SipStr::owned(name.as_wire_str()),
                value: SipStr::owned(&value),
            });
        }
    }
}

#[cfg(test)]
pub(crate) mod advertisement_tests {
    //! What the two B2BUA-owned INVITE mint points advertise (`Allow` /
    //! `Supported`): [`build_b_leg`] on the originated leg and
    //! [`stamp_a_facing_invite_advert`] on the response facing the originator.
    //! Every assertion reads the emitted header, not the declaration.
    use super::*;
    use crate::config::B2buaConfig;
    use crate::effects::OutboundBody;
    use crate::rules::relay::{build_b_leg, relay_request_passthrough_headers};
    use sip_message::generators;
    use sip_message::header::{self, Allow, HeaderValue, Supported, Via};
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser, SipRequest};
    use sip_txn::IdGen;

    fn a_leg_invite() -> SipRequest {
        a_leg_invite_carrying(&[])
    }

    /// Alice's INVITE, carrying `extra` header lines of her own.
    pub(crate) fn a_leg_invite_carrying(extra: &[(&str, &str)]) -> SipRequest {
        let mut raw = "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n"
            .to_string();
        for (name, value) in extra {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        raw.push_str("Content-Length: 0\r\n\r\n");
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// The `(Allow, Supported)` the originated-leg INVITE carries on the wire.
    fn b_leg_advert(
        capabilities: &CapabilitySet,
        header_updates: &[(String, Option<String>)],
    ) -> (Option<String>, Option<String>) {
        b_leg_advert_from(&a_leg_invite(), capabilities, header_updates)
    }

    /// The same, for an originator INVITE that advertises a set of its own.
    fn b_leg_advert_from(
        a_leg_invite: &SipRequest,
        capabilities: &CapabilitySet,
        header_updates: &[(String, Option<String>)],
    ) -> (Option<String>, Option<String>) {
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            a_leg_invite,
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0xCAB),
            None,
            header_updates,
            capabilities,
            None, // no charging vector
            &[],  // no withheld option tags
            None,
        )
        .expect("no identity rewrites, so nothing to refuse");
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) | OutboundBody::Datagram(_) => {
                panic!("b-leg effect must carry a request")
            }
        };
        let allow = invite.raw_text(HeaderName::Allow).next().map(|v| v.as_str().to_string());
        let supported =
            invite.raw_text(HeaderName::Supported).next().map(|v| v.as_str().to_string());
        (allow, supported)
    }

    /// The `(Allow, Supported)` the a-facing 2xx header set ends up carrying,
    /// starting from a callee value the B2BUA must replace.
    fn a_facing_advert(
        capabilities: &CapabilitySet,
        rule_stamped: &[Entry],
    ) -> (Option<String>, Option<String>) {
        let mut headers = vec![MsgHeader {
            name: SipStr::from_static("Supported"),
            value: SipStr::from_static("callees-own-tag"),
        }];
        stamp_a_facing_invite_advert(&mut headers, rule_stamped, capabilities);
        let value = |name: HeaderName| {
            headers.iter().find(|h| name.matches(&h.name)).map(|h| h.value.as_str().to_string())
        };
        (value(HeaderName::Allow), value(HeaderName::Supported))
    }

    /// An inbound re-INVITE carrying the peer's own `Supported`.
    fn peer_reinvite() -> SipRequest {
        let raw = "INVITE sip:b2bua@10.244.2.7:5080 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-reinvite\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>;tag=b2bua-to-tag\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 315 INVITE\r\n\
Supported: 100rel, timer, replaces\r\n\
Content-Length: 0\r\n\r\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// The `(Allow, Supported)` a RELAYED re-INVITE carries toward the target
    /// face, built exactly as the relay action builds it: the passthrough set
    /// as `extra_headers`, the face's set as `capabilities`.
    fn relayed_reinvite_advert(
        capabilities: &CapabilitySet,
        target_declared: &[HeaderName],
    ) -> (Option<String>, Option<String>) {
        let dialog = generators::StackDialog {
            call_id: "b-leg-call-id".to_string(),
            local_tag: "b2bua-local".to_string(),
            remote_tag: "bob-remote".to_string(),
            local_uri: "sip:b2bua@10.244.2.7:5080".to_string(),
            remote_uri: "sip:bob@10.0.0.2:5070".to_string(),
            remote_target: "sip:bob@10.0.0.2:5070".to_string(),
            local_cseq: 41,
            route_set: Vec::new(),
        };
        let opts = generators::GenerateInDialogRequestOpts {
            via: Some(
                Via::parse(&SipStr::from_static("SIP/2.0/UDP 10.244.2.7:5080;branch=z9hG4bK-b"))
                    .unwrap(),
            ),
            contact: Some(
                header::Contact::parse(&SipStr::from_static("<sip:b2bua@10.244.2.7:5080>"))
                    .unwrap(),
            ),
            extra_headers: relay_request_passthrough_headers(&peer_reinvite(), target_declared),
            capabilities: Some(capabilities.clone()),
            ..Default::default()
        };
        let out = generators::generate_in_dialog_request(
            generators::InDialogMethod::Invite,
            &dialog,
            &opts,
        )
        .request;
        let value = |name: HeaderName| out.raw_text(name).next().map(|v| v.as_str().to_string());
        (value(HeaderName::Allow), value(HeaderName::Supported))
    }

    /// A narrow set: no REFER/INFO/NOTIFY/PRACK, no 100rel, no timers.
    fn narrow() -> CapabilitySet {
        CapabilitySet::new(
            Allow::of(["INVITE", "ACK", "CANCEL", "BYE", "OPTIONS"]),
            Supported::of(["replaces"]),
        )
    }

    /// Declaring nothing, with nothing received to relay, advertises NOTHING
    /// on either face: no line, and a callee line the set does not state is
    /// not kept either.
    #[test]
    fn an_undeclared_call_with_nothing_to_relay_advertises_nothing_on_both_faces() {
        let silent = CapabilitySet::silent();
        assert_eq!(b_leg_advert(&silent, &[]), (None, None));
        assert_eq!(a_facing_advert(&silent, &[]), (None, None));
    }

    /// The node's own set, stated where a message answers on the stack's own
    /// behalf, reaches the wire byte for byte, `Accept` included.
    #[test]
    fn the_stack_set_reaches_the_wire_where_it_is_stated() {
        let default = CapabilitySet::default();
        let (allow, supported) = b_leg_advert(&default, &[]);
        assert_eq!(allow.as_deref(), Some(generators::B2BUA_ALLOW));
        assert_eq!(supported.as_deref(), Some(generators::B2BUA_SUPPORTED));
        let (allow, supported) = a_facing_advert(&default, &[]);
        assert_eq!(allow.as_deref(), Some(generators::B2BUA_ALLOW));
        assert_eq!(supported.as_deref(), Some(generators::B2BUA_SUPPORTED));
    }

    /// A declared set is what reaches the wire on the originated leg.
    #[test]
    fn the_declared_set_reaches_the_originated_leg() {
        let (allow, supported) = b_leg_advert(&narrow(), &[]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(supported.as_deref(), Some("replaces"));
    }

    /// A declared set is what reaches the wire toward the originator, replacing
    /// whatever the callee's 200 carried.
    #[test]
    fn the_declared_set_reaches_the_originator() {
        let (allow, supported) = a_facing_advert(&narrow(), &[]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(supported.as_deref(), Some("replaces"));
    }

    /// The two faces are independent: a bridge between asymmetric domains
    /// narrows the originated leg while the originator still sees the full set.
    #[test]
    fn the_two_faces_advertise_independently() {
        let (b_allow, b_supported) = b_leg_advert(&narrow(), &[]);
        let (a_allow, a_supported) = a_facing_advert(&CapabilitySet::default(), &[]);
        assert_eq!(b_allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(b_supported.as_deref(), Some("replaces"));
        assert_eq!(a_allow.as_deref(), Some(generators::B2BUA_ALLOW));
        assert_eq!(a_supported.as_deref(), Some(generators::B2BUA_SUPPORTED));
        assert_ne!(a_allow, b_allow, "the faces carry different Allow sets");
    }

    /// An explicit header update on the message is more specific than the
    /// call's declared set and wins; the declaration still supplies the header
    /// the update does not name.
    #[test]
    fn an_explicit_header_update_beats_the_declared_set_on_the_originated_leg() {
        let updates = vec![("Allow".to_string(), Some("INVITE, ACK, BYE, MESSAGE".to_string()))];
        let (allow, supported) = b_leg_advert(&narrow(), &updates);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, BYE, MESSAGE"));
        assert_eq!(supported.as_deref(), Some("replaces"));
    }

    /// Same precedence toward the originator: a value the firing rule stamped
    /// itself wins over the declared set, and stays the ONLY line of that name.
    #[test]
    fn a_rule_stamped_value_beats_the_declared_set_toward_the_originator() {
        let stamped = [Entry::typed(Supported::of(["timer"]))];
        let (allow, supported) = a_facing_advert(&narrow(), &stamped);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(
            supported.as_deref(),
            Some("callees-own-tag"),
            "the rule owns Supported here, so the pass-through line it placed is kept as-is"
        );
    }

    /// A declared set survives the RELAYED re-INVITE: the peer's `Supported`
    /// is a relayed value, not an explicit instruction, so it does not revert
    /// the narrowing that `Allow` (never relayed) keeps on the same message.
    #[test]
    fn a_declared_set_survives_a_relayed_reinvite() {
        let (allow, supported) = relayed_reinvite_advert(&narrow(), &[HeaderName::Supported]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE, OPTIONS"));
        assert_eq!(supported.as_deref(), Some("replaces"));
    }

    /// With no declaration the transparent relay stands: the peer's
    /// `Supported` still rides end to end (RFC 3262 negotiation), and the
    /// `Allow` the peer never sent is not invented on the relayed re-INVITE.
    #[test]
    fn an_undeclared_face_relays_the_peers_value_on_a_reinvite_and_invents_none() {
        let (allow, supported) = relayed_reinvite_advert(&CapabilitySet::silent(), &[]);
        assert_eq!(allow, None, "the peer stated no Allow, so none is stated onward");
        assert_eq!(supported.as_deref(), Some("100rel, timer, replaces"));
    }

    /// The originator's own advertisement travels onto the leg the B2BUA
    /// originates (RFC 3261 §16.6), token for token: every method she accepts
    /// is there, and nothing of the stack's own is added.
    #[test]
    fn the_originators_methods_reach_the_originated_leg_and_nothing_is_added() {
        let invite = a_leg_invite_carrying(&[("Allow", "INVITE, ACK, BYE, MESSAGE")]);
        let caps = crate::rules::capabilities::relaying_in(
            None,
            crate::rules::capabilities::Face::Originated,
            invite.headers(),
        );
        let (allow, supported) = b_leg_advert_from(&invite, &caps, &[]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK, BYE, MESSAGE"));
        assert_eq!(supported, None, "she stated no option tag, so none is claimed for her");
    }

    /// An `Accept` the originator sent reaches the originated leg as she
    /// stated it; one she did not send is not minted.
    #[test]
    fn the_originators_accept_reaches_the_originated_leg_verbatim() {
        let invite = a_leg_invite_carrying(&[(
            "Accept",
            "application/sdp, application/isup, application/xml",
        )]);
        let caps = crate::rules::capabilities::relaying_in(
            None,
            crate::rules::capabilities::Face::Originated,
            invite.headers(),
        );
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            &invite,
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0xCAB),
            None,
            &[],
            &caps,
            None,
            &[],
            None,
        )
        .expect("nothing to refuse");
        let OutboundBody::Request(out) = effect.body else { panic!("a request") };
        let accepts: Vec<String> =
            out.raw_text(HeaderName::Accept).map(|v| v.as_str().to_string()).collect();
        assert_eq!(accepts, ["application/sdp, application/isup, application/xml"]);

        let bare = a_leg_invite();
        let caps = crate::rules::capabilities::relaying_in(
            None,
            crate::rules::capabilities::Face::Originated,
            bare.headers(),
        );
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            &bare,
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0xCAB),
            None,
            &[],
            &caps,
            None,
            &[],
            None,
        )
        .expect("nothing to refuse");
        let OutboundBody::Request(out) = effect.body else { panic!("a request") };
        assert_eq!(out.raw_text(HeaderName::Accept).count(), 0, "no Accept is invented");
    }

    /// An option tag obliges whoever advertises it: the originated leg claims
    /// exactly what the originator claimed — the captured tag is not dropped,
    /// and `100rel`/`timer` are not invented on her behalf.
    #[test]
    fn the_originated_leg_claims_the_originators_option_tags_and_no_others() {
        let invite = a_leg_invite_carrying(&[("Supported", "path, gin")]);
        let caps = crate::rules::capabilities::relaying_in(
            None,
            crate::rules::capabilities::Face::Originated,
            invite.headers(),
        );
        let (_, supported) = b_leg_advert_from(&invite, &caps, &[]);
        assert_eq!(supported.as_deref(), Some("path, gin"));
    }

    /// A declaration is the more specific statement and still outranks the
    /// relayed set, half for half.
    #[test]
    fn a_declaration_outranks_the_originators_relayed_set() {
        let invite = a_leg_invite_carrying(&[("Allow", "INVITE, MESSAGE"), ("Supported", "path")]);
        let features = declaring_originated(Some(vec!["INVITE".into(), "ACK".into()]), None);
        let caps = crate::rules::capabilities::relaying_in(
            Some(&features),
            crate::rules::capabilities::Face::Originated,
            invite.headers(),
        );
        let (allow, supported) = b_leg_advert_from(&invite, &caps, &[]);
        assert_eq!(allow.as_deref(), Some("INVITE, ACK"), "the declared half stands");
        assert_eq!(supported.as_deref(), Some("path"), "the undeclared half relays");
    }

    /// Feature activations declaring a set toward the originated face.
    fn declaring_originated(
        allow: Option<Vec<String>>,
        supported: Option<Vec<String>>,
    ) -> call::features::FeatureActivations {
        call::features::FeatureActivations {
            platform: call::features::PlatformActivations {
                max_duration_sec: 3_600,
                keepalive: call::features::KeepaliveActivation { interval_sec: 30, max_missed: 2 },
            },
            refer: None,
            relay_first_18x_to_180: None,
            no_answer_timeout_sec: None,
            call_limiters: None,
            charging_vector: None,
            withhold_option_tags: None,
            advertise_capabilities: Some(call::features::AdvertiseCapabilitiesFeature {
                toward_originator: None,
                toward_originated: Some(call::features::AdvertisedCapabilities {
                    allow,
                    supported,
                }),
            }),
        }
    }
}
