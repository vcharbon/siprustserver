//! Wire-output pins on [`build_b_leg`]: per-leg dialog-identity independence
//! (ID-1/2/3, HDR-2), the §16.6 relay onto the originated leg, and RFC 7315
//! charging correlation — every assertion reads the serialized INVITE.

use sip_message::generators::CapabilitySet;
use sip_message::header::{ChargingVector, HeaderValue};
use sip_message::{SipHeader as MsgHeader, SipRequest, SipStr};
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::effects::OutboundBody;
use crate::rules::relay::build_b_leg;

mod identity_tests {
    //! Per-leg identity invariants (ID-1/2/3, HDR-2) — the core back-to-back-UA
    //! property: the B2BUA mints the b-leg's dialog identity from scratch, copying
    //! *nothing* dialog-identifying from the a-leg. A b-leg whose Call-ID / From-tag
    //! / CSeq / Contact leaked from the a-leg would couple the two dialogs and is
    //! exactly what a transparent proxy (not a B2BUA) would do. Asserted directly
    //! against [`build_b_leg`] (the single mint point, `super::originate`).
    use super::*;
    use sip_message::header::Contact;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    fn parse(raw: &str) -> SipRequest {
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// A representative inbound a-leg INVITE with its OWN Call-ID, From-tag, an
    /// absent To-tag (initial INVITE), CSeq 314 and a caller-owned Contact user.
    fn a_leg_invite() -> SipRequest {
        parse(
            "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Contact: <sip:alice@192.0.2.5:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n\
Content-Length: 0\r\n\r\n",
        )
    }

    /// The b-leg INVITE's Contact.
    fn contact_of(req: &SipRequest) -> Contact {
        req.header::<Contact>().expect("b-leg INVITE has a Contact").expect("a readable Contact")
    }

    // The b-leg INVITE's dialog identity is independent of the a-leg's: a fresh
    // Call-ID, a fresh From-tag, no To-tag, CSeq 1, and the B2BUA's own Contact.
    #[test]
    fn b_leg_identity_is_independent_of_a_leg() {
        let a = a_leg_invite();
        let config = B2buaConfig::default(); // sip_local_ip = 127.0.0.1
        let id_gen = IdGen::seeded(0xB2B);

        let (leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false, // non-emergency
            &a,
            ("10.244.2.7".to_string(), 5060),
            None, // R-URI defaults to a-leg's
            None, // From URI relayed from a-leg
            None, // To URI relayed from a-leg
            None, // no NoAnswer
            &config,
            &id_gen,
            None, // no body override
            &[],  // no header updates
            &CapabilitySet::default(), // undeclared → the stack capability set
            None,
            None, // Destination leg
        )
        .expect("no identity rewrites, so nothing to refuse");

        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };

        // (a) ID-1 — fresh Call-ID, NOT the a-leg's.
        let call_id = invite.call_id();
        let call_id = call_id.as_str();
        assert_ne!(call_id, a.call_id().as_str(), "b-leg Call-ID must not be the a-leg's");
        assert_eq!(call_id, leg.call_id, "leg + INVITE Call-IDs agree");
        // The mint shape is `<leg>-<tag>@<local_ip>` (the `build_b_leg` mint), so it carries the
        // leg id and the B2BUA's own host — never alice's Call-ID host.
        assert!(call_id.starts_with("b-1-"), "Call-ID carries the leg id: {call_id}");
        assert!(call_id.ends_with("@127.0.0.1"), "Call-ID host is the B2BUA's: {call_id}");

        // (b) ID-2 — fresh From-tag (B2BUA-owned), NOT the a-leg's; To-tag absent
        // on the initial INVITE (the callee mints it in its 2xx, RFC 3261 §12.1.1).
        let from = invite.from();
        let from_tag = from.tag().expect("b-leg From carries a tag");
        assert_ne!(from_tag, "alice-from-tag", "b-leg From-tag must not be the a-leg's");
        assert_eq!(from_tag, leg.from_tag, "leg + INVITE From-tags agree");
        let to = invite.to();
        assert!(to.tag().is_none(), "initial b-leg INVITE has no To-tag, got {:?}", to.tag());

        // (c) ID-3 — the b-leg dialog starts a fresh CSeq space at 1 (the a-leg's
        // INVITE was CSeq 314).
        assert_eq!(invite.cseq().seq(), 1, "b-leg CSeq starts at 1, independent of the a-leg's 314");

        // (d) HDR-2 — Contact is the B2BUA's own address (host = local_ip), and its
        // user is the B2BUA's, NOT the a-leg caller's ("alice").
        let contact = contact_of(&invite);
        let cuser = contact.uri().user().unwrap_or("");
        assert_ne!(cuser, "alice", "b-leg Contact user must not be the a-leg caller's");
        assert_eq!(cuser, "b2bua", "b-leg Contact is the B2BUA's own identity");
        assert_eq!(
            contact.uri().host(),
            "127.0.0.1",
            "b-leg Contact host must be the B2BUA local addr: {}",
            contact.uri()
        );
    }

    /// Build the initial b-leg INVITE for `is_emergency` and return its raw Via +
    /// Contact header values (the on-the-wire surface, not the builder structs).
    fn b_leg_invite_via_contact(is_emergency: bool) -> (String, String) {
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            is_emergency,
            &a_leg_invite(),
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0xE3E),
            None,
            &[],
            &CapabilitySet::default(),
            None, // no charging vector
            None,
        )
        .expect("no identity rewrites, so nothing to refuse");
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };
        (
            invite.top_via().to_string(),
            contact_of(&invite).to_wire(),
        )
    }

    // The wiring contract: an EMERGENCY call's initial b-leg INVITE (the single
    // mint point) carries `;em=1` on its Via and `;emerg=1` on its Contact ON
    // THE WIRE, so the call stays identifiable in-dialog. The pure-builder tests
    // prove the `if is_emergency` branch in isolation; this proves a production
    // relay path actually passes `true` and the markers reach the serialized
    // message.
    #[test]
    fn emergency_b_leg_invite_via_and_contact_carry_the_markers() {
        let (via, contact) = b_leg_invite_via_contact(true);
        assert!(via.contains(";em=1"), "emergency b-leg Via must carry ;em=1: {via}");
        assert!(
            contact.contains(";emerg=1"),
            "emergency b-leg Contact must carry ;emerg=1: {contact}"
        );
    }

    // A non-emergency call's b-leg INVITE carries NEITHER marker (the markers are
    // strictly an emergency signal — stamping them on a normal call would exempt
    // it from overload shedding).
    #[test]
    fn non_emergency_b_leg_invite_omits_the_markers() {
        let (via, contact) = b_leg_invite_via_contact(false);
        assert!(!via.contains(";em=1"), "non-emergency Via must NOT carry ;em=1: {via}");
        assert!(
            !contact.contains(";emerg=1"),
            "non-emergency Contact must NOT carry ;emerg=1: {contact}"
        );
    }

    /// An a-leg INVITE carrying, alongside its structural headers: an extension
    /// header and an unmodelled vendor one (both relayable), a `Record-Route`
    /// (alice's route set is not the callee's to learn), and one member of each
    /// withheld class.
    fn a_leg_invite_with_relay_header() -> SipRequest {
        parse(
            "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
Record-Route: <sip:proxy.alice.example;lr>\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Contact: <sip:alice@192.0.2.5:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n\
X-Loadgen-Id: lg-abc123\r\n\
P-Charging-Vector: icid-value=\"icid-from-alice\"\r\n\
Authorization: Digest username=\"alice\",realm=\"alice.example\"\r\n\
Session-Expires: 1800;refresher=uac\r\n\
Timestamp: 54\r\n\
Replaces: other-call-id;to-tag=t;from-tag=f\r\n\
Require: precondition\r\n\
Content-Length: 0\r\n\r\n",
        )
    }

    /// Build the originated b-leg INVITE for the given relay config + R-URI and
    /// return its parsed headers. `new_ruri` lets one helper drive BOTH the
    /// normal callee leg (`None` → keeps the a-leg R-URI / bob) and the REFER
    /// transfer leg (`Some(charlie)` → the rebuilt a-leg invite re-aimed).
    fn relay_b_leg_headers(relay_headers: Vec<String>, new_ruri: Option<&str>) -> Vec<MsgHeader> {
        let config = B2buaConfig { relay_headers, ..B2buaConfig::default() };
        let (_leg, effect) = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            &a_leg_invite_with_relay_header(),
            ("10.244.2.7".to_string(), 5060),
            new_ruri,
            None,
            None,
            None,
            &config,
            &IdGen::seeded(0x4747),
            None,
            &[],
            &CapabilitySet::default(),
            None, // no charging vector
            None,
        )
        .expect("the R-URI under test reads");
        match effect.body {
            OutboundBody::Request(r) => r.headers().to_vec(),
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        }
    }

    /// RFC 3261 §16.6: a header alice sent that the generator does not own rides
    /// onto BOTH originated legs — the callee leg and the REFER transfer leg,
    /// which is the same mint point fed alice's rehydrated INVITE. The
    /// generator-owned headers stay the generator's: naming `To` in the
    /// configured list still yields exactly one, structurally minted `To`, and
    /// alice's `Record-Route`/`Contact`/`Via` never reach the callee.
    #[test]
    fn a_received_header_rides_both_originated_legs_and_never_the_owned_ones() {
        let has = |hs: &[MsgHeader], name: &str, val: &str| {
            hs.iter().any(|h| h.name.eq_ignore_ascii_case(name) && h.value == val)
        };
        let count = |hs: &[MsgHeader], name: &str| {
            hs.iter().filter(|h| h.name.eq_ignore_ascii_case(name)).count()
        };

        // (a) NORMAL callee leg (bob), with nothing configured: the relay carries
        //     the extension header and the vendor header it has no model for.
        let bob = relay_b_leg_headers(Vec::new(), None);
        assert!(has(&bob, "X-Loadgen-Id", "lg-abc123"), "callee leg carries it: {bob:?}");
        assert!(
            has(&bob, "P-Charging-Vector", "icid-value=\"icid-from-alice\""),
            "an unmodelled header rides verbatim: {bob:?}"
        );

        // (b) REFER transfer leg (charlie): same mint point, R-URI re-aimed.
        let charlie = relay_b_leg_headers(Vec::new(), Some("sip:charlie@10.244.2.9:5060"));
        assert!(has(&charlie, "X-Loadgen-Id", "lg-abc123"), "transfer leg carries it");

        // (c) The generator's own headers are the generator's: alice's route set
        //     and her Contact are hers, and her Via would be a routing loop.
        assert_eq!(count(&bob, "Record-Route"), 0, "alice's route set stays alice's: {bob:?}");
        assert_eq!(count(&bob, "Via"), 1, "exactly the b-leg's own Via: {bob:?}");
        assert!(
            !has(&bob, "Contact", "<sip:alice@192.0.2.5:5060>"),
            "the b-leg Contact is this stack's: {bob:?}"
        );

        // (d) Naming a generator-owned header in the configured list cannot
        //     duplicate it — configuration does not reach past §16.6.
        let with_to = relay_b_leg_headers(vec!["To".into()], None);
        assert_eq!(count(&with_to, "To"), 1, "one structural To, never a relayed dup: {with_to:?}");
    }

    /// The withheld classes do not ride the originated INVITE: a credential
    /// scoped to alice's realm, a session interval negotiated with alice, her
    /// own clock stamp, a dialog identifier this stack re-mints, and a
    /// requirement this stack already accepted as the UAS.
    #[test]
    fn the_withheld_classes_never_reach_the_callee() {
        let bob = relay_b_leg_headers(Vec::new(), None);
        for name in
            ["Authorization", "Session-Expires", "Timestamp", "Replaces", "Require"]
        {
            assert!(
                !bob.iter().any(|h| h.name.eq_ignore_ascii_case(name)),
                "{name} must not reach the callee: {bob:?}"
            );
        }
    }

    /// Precedence at the originated-leg mint point: a decision's explicit header
    /// update is more specific than the relayed value and wins, as exactly one
    /// line of that name.
    #[test]
    fn an_explicit_header_update_beats_the_relayed_value() {
        let updates =
            vec![("X-Loadgen-Id".to_string(), Some("stated-by-the-decision".to_string()))];
        let bob = build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            &a_leg_invite_with_relay_header(),
            ("10.244.2.7".to_string(), 5060),
            None,
            None,
            None,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0x4747),
            None,
            &updates,
            &CapabilitySet::default(),
            None, // no charging vector
            None,
        )
        .map(|(_leg, effect)| match effect.body {
            OutboundBody::Request(r) => r.headers().to_vec(),
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        })
        .expect("no identity rewrites, so nothing to refuse");
        let stated: Vec<&str> = bob
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("X-Loadgen-Id"))
            .map(|h| h.value.as_str())
            .collect();
        assert_eq!(stated, ["stated-by-the-decision"], "the update wins, alone: {bob:?}");
    }
}

mod charging_tests {
    //! RFC 7315 §5.6 charging correlation on a leg the B2BUA originates.
    use super::super::advert::advertisement_tests::a_leg_invite_carrying;
    use super::*;
    use call::features::ChargingVectorFeature;

    /// The `P-Charging-Vector` the originated-leg INVITE carries, if any.
    fn b_leg_vector(
        a_leg_invite: &SipRequest,
        charging: Option<&ChargingVectorFeature>,
    ) -> Option<String> {
        b_leg_vector_from(a_leg_invite, charging, &IdGen::seeded(0xCAB))
    }

    /// The same, on a stated generator — one worker's identifier stream.
    fn b_leg_vector_from(
        a_leg_invite: &SipRequest,
        charging: Option<&ChargingVectorFeature>,
        id_gen: &IdGen,
    ) -> Option<String> {
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
            id_gen,
            None,
            &[],
            &CapabilitySet::default(),
            charging,
            None,
        )
        .expect("no identity rewrites, so nothing to refuse");
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) => panic!("b-leg effect must carry a request"),
        };
        let name = ChargingVector::header_name();
        invite
            .headers()
            .iter()
            .find(|h| name.matches(&h.name))
            .map(|h| h.value.as_str().to_string())
    }

    /// Armed and nothing received: the element starting the leg generates the
    /// identifier, stating where it generated it.
    #[test]
    fn an_originated_leg_carries_a_generated_identifier() {
        let value = b_leg_vector(&a_leg_invite_carrying(&[]), Some(&ChargingVectorFeature::default()))
            .expect("an armed call stamps a charging vector");
        let parsed = ChargingVector::parse(&SipStr::owned(&value)).expect("RFC 7315 §5.6 form");
        assert!(!parsed.icid_value().is_empty());
        assert_eq!(parsed.icid_generated_at(), Some(B2buaConfig::default().sip_local_ip.as_str()));
    }

    /// Two legs of two calls never share an identifier — it is the key the
    /// records are matched on.
    #[test]
    fn each_originated_leg_generates_its_own_identifier() {
        let arm = ChargingVectorFeature::default();
        let id_gen = IdGen::seeded(0xCAB);
        let first = b_leg_vector_from(&a_leg_invite_carrying(&[]), Some(&arm), &id_gen);
        let second = b_leg_vector_from(&a_leg_invite_carrying(&[]), Some(&arm), &id_gen);
        assert_ne!(first, second);
    }

    /// The correlation invariant: a vector the originator sent is the session's
    /// identifier, relayed unchanged — an armed call never re-mints it.
    #[test]
    fn a_received_identifier_is_relayed_unchanged_even_when_armed() {
        let received = "icid-value=abc123;icid-generated-at=upstream.example";
        let invite = a_leg_invite_carrying(&[("P-Charging-Vector", received)]);
        assert_eq!(
            b_leg_vector(&invite, Some(&ChargingVectorFeature::default())).as_deref(),
            Some(received)
        );
    }

    /// Unarmed: the stack generates none, and a received one still relays.
    #[test]
    fn an_unarmed_call_generates_none() {
        assert_eq!(b_leg_vector(&a_leg_invite_carrying(&[]), None), None);
        let received = "icid-value=abc123";
        let invite = a_leg_invite_carrying(&[("P-Charging-Vector", received)]);
        assert_eq!(b_leg_vector(&invite, None).as_deref(), Some(received));
    }

    /// The arm names the element the identifier is generated at.
    #[test]
    fn the_arm_names_the_generating_element() {
        let arm = ChargingVectorFeature { generated_at: Some("edge.example".to_string()) };
        let value = b_leg_vector(&a_leg_invite_carrying(&[]), Some(&arm)).expect("armed");
        let parsed = ChargingVector::parse(&SipStr::owned(&value)).unwrap();
        assert_eq!(parsed.icid_generated_at(), Some("edge.example"));
    }
}
