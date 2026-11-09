//! The type of the body [`build_b_leg`] mints (RFC 3261 §20.15): an override
//! is typed by what its author states or by its own bytes, never by the
//! framing of the originator's body, and none of her entity headers ride
//! beside bytes she did not write. The via-extension's offer flag reads the
//! minted body as `sdp()` does.

use sip_ids::TurnIds;
use sip_message::generators::CapabilitySet;
use sip_message::SipRequest;

use crate::config::B2buaConfig;
use crate::effects::OutboundBody;
use crate::rules::relay::{build_b_leg, CallMarks};
use b2bua_sdk::model::Body;
use sip_message::header::{HeaderName, MediaType};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=b 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";

/// An originator INVITE framing its offer beside another part.
fn multipart_invite() -> SipRequest {
    let body = "--X\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n\r\n--X\r\nContent-Type: application/isup\r\nContent-Disposition: signal;handling=optional\r\n\r\nAB\r\n--X--\r\n";
    let raw = format!(
        "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5>;tag=a\r\n\
To: <sip:bob@10.244.2.7>\r\n\
Contact: <sip:alice@192.0.2.5>\r\n\
Call-ID: mp@192.0.2.5\r\n\
CSeq: 1 INVITE\r\n\
MIME-Version: 1.0\r\n\
Content-Disposition: session\r\n\
Content-Type: multipart/mixed;boundary=X\r\n\
Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => unreachable!(),
    }
}

fn minted(a: &SipRequest, body: &Body) -> SipRequest {
    minted_under(&B2buaConfig::default(), a, Some(body))
}

fn minted_under(config: &B2buaConfig, a: &SipRequest, body: Option<&Body>) -> SipRequest {
    let (_leg, effect) = build_b_leg(
        CallMarks {
            call_ref: "w0|call-ref|xyz",
            incarnation_mark: "k",
            is_emergency: false,
            owners: crate::owners::Owners::none(),
        },
        "b-2",
        a,
        ("10.244.2.7".to_string(), 5060),
        None,
        None,
        None,
        config,
        &mut TurnIds::fixed(0x0FF),
        body,
        &[],
        &CapabilitySet::default(),
        None,
        &[],
        &[],
        None,
        false,
        0,
        &mut Vec::new(),
    )
    .expect("nothing to refuse");
    match effect.body {
        OutboundBody::Request(r) => r,
        _ => panic!("a request"),
    }
}

#[test]
fn an_override_typed_for_itself_carries_none_of_the_originators_framing() {
    let invite = minted(
        &multipart_invite(),
        &Body::own(OFFER.as_bytes().to_vec(), Some("application/sdp".into())),
    );
    let ct = invite.header::<MediaType>().expect("a Content-Type").expect("it reads");
    assert!(ct.is_sdp(), "{}", ct.token());
    assert_eq!(invite.body(), OFFER.as_bytes());
    for name in ["MIME-Version", "Content-Disposition"] {
        assert!(invite.raw(HeaderName::from(name)).next().is_none(), "no {name}");
    }
}

/// An override stating no type is a description of this stack's (RFC 3261
/// §20.15): typed `application/sdp` whatever framed the originator's body,
/// with none of her lines describing that body.
#[test]
fn an_untyped_override_is_typed_as_a_description() {
    let invite = minted(&multipart_invite(), &Body::own(OFFER.as_bytes().to_vec(), None));
    let ct = invite.header::<MediaType>().expect("a Content-Type").expect("it reads");
    assert!(ct.is_sdp(), "{}", ct.token());
    assert_eq!(invite.sdp(), Some(OFFER.as_bytes()));
    for name in ["MIME-Version", "Content-Disposition"] {
        assert!(invite.raw(HeaderName::from(name)).next().is_none(), "no {name}");
    }
}

/// An untyped override holding the originator's own bytes is her body:
/// her type and role ride.
#[test]
fn an_untyped_override_of_the_originators_bytes_keeps_her_type() {
    let a = multipart_invite();
    let invite = minted(&a, &Body::own(a.body().to_vec(), None));
    let ct = invite.header::<MediaType>().expect("a Content-Type").expect("it reads");
    assert!(ct.is("multipart/mixed"), "{}", ct.token());
    assert_eq!(invite.raw(HeaderName::ContentDisposition).collect::<Vec<_>>(), ["session"]);
}

/// The via-extension's offer flag reads the minted body as `sdp()` does:
/// a body whose only description is disposed `early-session` makes no
/// offer (RFC 3959), so a 2xx answered on the unheld path draws its answer.
#[test]
fn the_via_offer_flag_reads_the_minted_description() {
    let key = sip_ids::SigningKey::new(&[3u8; 32]).expect("a key");
    let config = B2buaConfig { cluster_key: Some(key.clone()), ..B2buaConfig::default() };
    let offer_of = |invite: &SipRequest| {
        crate::stack_identity::read_via_extension(invite.top_via(), "INVITE", Some(&key))
            .expect("a verified extension")
            .offer
    };
    let early = invite_with(&[("Content-Disposition", "early-session")], OFFER);
    assert!(!offer_of(&minted_under(&config, &early, None)), "early-session is no offer");
    let session = invite_with(&[], OFFER);
    assert!(offer_of(&minted_under(&config, &session, None)), "a description is an offer");
    let text = Body::own(b"a note".to_vec(), Some("text/plain".into()));
    assert!(!offer_of(&minted_under(&config, &session, Some(&text))), "text is no offer");
}

/// An originator INVITE carrying `body` typed `application/sdp`, beside
/// `lines`.
fn invite_with(lines: &[(&str, &str)], body: &str) -> SipRequest {
    let stated: String = lines.iter().map(|(n, v)| format!("{n}: {v}\r\n")).collect();
    let raw = format!(
        "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5>;tag=a\r\n\
To: <sip:bob@10.244.2.7>\r\n\
Contact: <sip:alice@192.0.2.5>\r\n\
Call-ID: es@192.0.2.5\r\n\
CSeq: 1 INVITE\r\n\
{stated}Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => unreachable!(),
    }
}
