//! Facts one captured message yields: its Via chain, an allow-listed header
//! projection, the user identities it names, the dialogs it references and the
//! layout of its body.
//!
//! Every read goes through `sip-message`; this module never looks at header or
//! body bytes itself — the RFC 2046 boundary walk lives beside its compose
//! mirror in `sip_message::multipart`, and this module keeps only the
//! projection into the document.

use sip_message::header::{HeaderName, HeaderValue, ParamValue, Uri, Wire};
use sip_message::{header, SipMessage, SipStr};

use crate::doc::{
    BodyJson, DialogRef, HeaderJson, Identities, Identity, MsgJson, ReferToJson, ViaJson,
};

/// Compute every enrichment field of `msg` and write it onto `out`.
pub fn apply(msg: &SipMessage, allow: &[HeaderName], out: &mut MsgJson) {
    out.via = via_chain(msg);
    out.headers = projected_headers(msg, allow);
    out.identities = identities(msg);
    out.rseq = msg.raw(HeaderName::RSeq).next().map(|v| v.trim().to_string());
    out.replaces = replaces_of(msg);
    out.refer_to = refer_to_of(msg);
    out.body = BodyJson::of(msg);
}

fn via_chain(msg: &SipMessage) -> Vec<ViaJson> {
    msg.via()
        .iter()
        .map(|v| ViaJson {
            sent_by: render(|w| v.sent_by().render(w)),
            transport: v.transport().to_string(),
            branch: v.branch().map(str::to_string),
            received: v.received().map(str::to_string),
        })
        .collect()
}

/// The allow-listed headers this message carries, in wire order, duplicates
/// kept. A header is matched by IDENTITY, so a compact spelling is projected
/// under the canonical name the allow-list asked for.
fn projected_headers(msg: &SipMessage, allow: &[HeaderName]) -> Vec<HeaderJson> {
    if allow.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for h in msg.headers() {
        let seen = HeaderName::from(h.name.as_str());
        let Some(want) = allow.iter().find(|a| a.same_header(&seen)) else { continue };
        let name = want.as_wire_str();
        out.push(HeaderJson {
            name: name.to_string(),
            wire: (h.name.as_str() != name).then(|| h.name.as_str().to_string()),
            value: h.value.as_str().to_string(),
        });
    }
    out
}

fn identities(msg: &SipMessage) -> Identities {
    Identities {
        from: identity(msg.from().uri()),
        to: identity(msg.to().uri()),
        ruri: match msg {
            SipMessage::Request(r) => Some(identity(r.request_uri())),
            SipMessage::Response(_) => None,
        },
        pai: msg
            .list::<header::PAssertedIdentity>()
            .unwrap_or_default()
            .iter()
            .map(|p| identity(p.uri()))
            .collect(),
    }
}

fn identity(uri: &Uri) -> Identity {
    Identity { uri: uri.text().into_owned(), user: uri.user_identity(), digits: uri.user_digits() }
}

fn replaces_of(msg: &SipMessage) -> Option<DialogRef> {
    msg.header::<header::Replaces>()?.ok().map(dialog_ref)
}

/// `Refer-To: <sip:x@h?Replaces=cid%3Bto-tag%3Da%3Bfrom-tag%3Db>` — the target
/// plus the dialog an attended transfer names inside the escaped query.
fn refer_to_of(msg: &SipMessage) -> Option<ReferToJson> {
    let refer_to = msg.header::<header::ReferTo>()?.ok()?;
    let uri = refer_to.uri();
    let replaces = uri
        .escaped_header("Replaces")
        .and_then(|raw| header::Replaces::parse(&SipStr::owned(&raw)).ok())
        .map(dialog_ref);
    Some(ReferToJson { target: identity(uri), replaces })
}

fn dialog_ref(r: header::Replaces) -> DialogRef {
    DialogRef {
        call_id: r.token().to_string(),
        to_tag: r.param("to-tag").and_then(ParamValue::as_str).map(str::to_string),
        from_tag: r.param("from-tag").and_then(ParamValue::as_str).map(str::to_string),
    }
}

fn render(f: impl FnOnce(&mut Wire)) -> String {
    let mut w = Wire::new();
    f(&mut w);
    w.as_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::parser::SipParser;
    use sip_message::CustomParser;

    fn parse(bytes: &[u8]) -> SipMessage {
        CustomParser::new().parse(bytes).expect("test message parses")
    }

    fn blank(msg: &SipMessage) -> MsgJson {
        MsgJson::new(
            0,
            "10.0.0.1:5060".into(),
            "10.0.0.2:5060".into(),
            0,
            crate::doc::Payload::Text { raw: String::new() },
            crate::doc::Summary::Response {
                status: 0,
                reason: String::new(),
                cseq: crate::doc::CSeqJson { seq: msg.cseq().seq(), method: String::new() },
                from: crate::doc::Party { uri: String::new(), tag: None },
                to: crate::doc::Party { uri: String::new(), tag: None },
            },
        )
    }

    fn facts(bytes: &[u8], allow: &[&str]) -> MsgJson {
        let msg = parse(bytes);
        let allow: Vec<HeaderName> = allow.iter().map(|n| HeaderName::from(*n)).collect();
        let mut out = blank(&msg);
        apply(&msg, &allow, &mut out);
        out
    }

    const REFER: &[u8] = b"REFER sip:+33123@h SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK2;received=10.0.0.9\r\n\
v: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
Max-Forwards: 70\r\n\
From: <sip:0033900@h>;tag=f1\r\n\
To: <sip:+33123@h;npdi>\r\n\
Call-ID: refer-1\r\n\
CSeq: 2 REFER\r\n\
P-Asserted-Identity: <tel:+41319852573>\r\n\
X-Api-Call: call-9\r\n\
x-api-call: call-10\r\n\
Refer-To: <sip:+33456@h?Replaces=abc%40h%3Bto-tag%3Dtt%3Bfrom-tag%3Dff>\r\n\
l: 0\r\n\r\n";

    /// The Via chain reads top-first, compact spellings included, with the
    /// branch and the stamped `received` each hop carries.
    #[test]
    fn via_chain_is_ordered_top_first_with_branches() {
        let m = facts(REFER, &[]);
        assert_eq!(m.via.len(), 2);
        assert_eq!(m.via[0].sent_by, "10.0.0.2:5060");
        assert_eq!(m.via[0].branch.as_deref(), Some("z9hG4bK2"));
        assert_eq!(m.via[0].received.as_deref(), Some("10.0.0.9"));
        assert_eq!(m.via[1].sent_by, "10.0.0.1:5060");
        assert_eq!(m.via[1].received, None);
        assert_eq!(m.via[1].transport, "UDP");
    }

    /// Only allow-listed headers project; wire order and duplicates survive,
    /// and a differing wire spelling is kept beside the canonical name.
    #[test]
    fn the_header_projection_is_the_allow_list_in_wire_order() {
        let m = facts(REFER, &["X-Api-Call", "Refer-To", "Content-Length"]);
        let seen: Vec<(&str, Option<&str>, &str)> = m
            .headers
            .iter()
            .map(|h| (h.name.as_str(), h.wire.as_deref(), h.value.as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("X-Api-Call", None, "call-9"),
                ("X-Api-Call", Some("x-api-call"), "call-10"),
                ("Refer-To", None, "<sip:+33456@h?Replaces=abc%40h%3Bto-tag%3Dtt%3Bfrom-tag%3Dff>"),
                // `l` resolved to Content-Length before the projection saw it.
                ("Content-Length", None, "0"),
            ]
        );
        assert!(facts(REFER, &[]).headers.is_empty(), "no allow-list, no projection");
    }

    /// Identities carry the raw URI, the canonical user and the digit form —
    /// user-parameters and dial prefixes never reach the digits.
    #[test]
    fn identities_normalize_users_beside_the_raw_uris() {
        let m = facts(REFER, &[]);
        assert_eq!(m.identities.from.uri, "sip:0033900@h");
        assert_eq!(m.identities.from.digits.as_deref(), Some("33900"));
        assert_eq!(m.identities.to.uri, "sip:+33123@h;npdi");
        assert_eq!(m.identities.to.user.as_deref(), Some("+33123"));
        assert_eq!(m.identities.to.digits.as_deref(), Some("33123"));
        assert_eq!(m.identities.ruri.as_ref().unwrap().digits.as_deref(), Some("33123"));
        assert_eq!(m.identities.pai.len(), 1);
        assert_eq!(m.identities.pai[0].digits.as_deref(), Some("41319852573"));
    }

    /// A Refer-To's escaped `?Replaces=` is resolved to the dialog it names.
    #[test]
    fn refer_to_resolves_its_escaped_replaces() {
        let m = facts(REFER, &[]);
        let refer = m.refer_to.expect("REFER carries a Refer-To");
        assert_eq!(refer.target.digits.as_deref(), Some("33456"));
        let replaces = refer.replaces.expect("attended transfer names a dialog");
        assert_eq!(replaces.call_id, "abc@h");
        assert_eq!(replaces.to_tag.as_deref(), Some("tt"));
        assert_eq!(replaces.from_tag.as_deref(), Some("ff"));
        assert!(m.replaces.is_none(), "the dialog ref rides inside Refer-To, not beside it");
    }

    /// A `Replaces` header of its own is read as the dialog it replaces.
    #[test]
    fn a_replaces_header_names_the_replaced_dialog() {
        let invite = b"INVITE sip:b@h SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
From: <sip:a@h>;tag=f1\r\n\
To: <sip:b@h>\r\n\
Call-ID: att-1\r\n\
CSeq: 1 INVITE\r\n\
Replaces: 425928@bob.example;to-tag=7743;from-tag=6472\r\n\
Content-Length: 0\r\n\r\n";
        let r = facts(invite, &[]).replaces.expect("Replaces present");
        assert_eq!(r.call_id, "425928@bob.example");
        assert_eq!(r.to_tag.as_deref(), Some("7743"));
        assert_eq!(r.from_tag.as_deref(), Some("6472"));
    }

    /// A multipart body is located part by part: every part's content is a
    /// contiguous slice of the body, byte-exact, and the closing delimiter and
    /// preamble contribute nothing.
    #[test]
    fn multipart_parts_locate_their_content_byte_exactly() {
        let body = "\r\n--bnd\r\n\
Content-Type: application/sdp\r\n\r\n\
v=0\r\n\
--bnd\r\n\
Content-Type: application/octet-stream\r\n\
Content-ID: <msd@x>\r\n\
Content-Transfer-Encoding: binary\r\n\
Content-Disposition: signal;handling=optional\r\n\r\n\
BIN\r\n\
--bnd--\r\n";
        let raw = format!(
            "INVITE sip:b@h SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
From: <sip:a@h>;tag=f1\r\n\
To: <sip:b@h>\r\n\
Call-ID: mp-1\r\n\
CSeq: 1 INVITE\r\n\
Content-Type: multipart/mixed;boundary=bnd\r\n\
Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let msg = parse(raw.as_bytes());
        let mut out = blank(&msg);
        apply(&msg, &[], &mut out);
        let b = out.body.expect("multipart body");
        assert_eq!(b.content_type, "multipart/mixed");
        assert_eq!(b.len, body.len());
        assert_eq!(b.parts.len(), 2);
        let slice = |p: &crate::doc::PartJson| &body.as_bytes()[p.offset..p.offset + p.len];
        assert_eq!(b.parts[0].content_type, "application/sdp");
        assert_eq!(b.parts[0].content_id, None);
        assert!(b.parts[0].headers.is_empty());
        assert_eq!(slice(&b.parts[0]), b"v=0");
        assert_eq!(b.parts[1].content_type, "application/octet-stream");
        assert_eq!(b.parts[1].content_id.as_deref(), Some("<msd@x>"));
        // The entity headers beyond the two with their own field, in wire
        // order: what a byte-exact replay has to put back.
        assert_eq!(
            b.parts[1]
                .headers
                .iter()
                .map(|h| (h.name.as_str(), h.value.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Content-Transfer-Encoding", "binary"),
                ("Content-Disposition", "signal;handling=optional")
            ]
        );
        assert_eq!(slice(&b.parts[1]), b"BIN");
    }

    /// A single-part body states its media type and length and no parts.
    #[test]
    fn a_single_part_body_states_its_media_type_and_no_parts() {
        let raw = b"INVITE sip:b@h SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
From: <sip:a@h>;tag=f1\r\n\
To: <sip:b@h>\r\n\
Call-ID: sdp-1\r\n\
CSeq: 1 INVITE\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 3\r\n\r\nv=0";
        let b = facts(raw, &[]).body.expect("body present");
        assert_eq!(b.content_type, "application/sdp");
        assert_eq!(b.len, 3);
        assert!(b.parts.is_empty());
    }
}
