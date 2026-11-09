//! The answer rejecting every stream of an offer (RFC 3264 §6).

use super::{push_line, with_port_zero};
use crate::sdp::{sdp_origin_address, sdp_session_id, BuildHeldSdpOptions};
use crate::sdp_doc::{parse_sdp_body, Sections};

/// The answer to `offer` rejecting each of its streams (RFC 3264 §6): every
/// offered m-line with port 0, its formats and `a=inactive`, under an origin
/// and a session-level `c=` of the answerer's own (`options`' address, a
/// session id from its clock). `None` where `offer` is not a session
/// description.
pub fn reject_offer(offer: &[u8], options: &BuildHeldSdpOptions) -> Option<Vec<u8>> {
    let doc = parse_sdp_body(offer)?;
    let text = crate::sdp_octets::decode(offer)?;
    let sections = Sections::of(&text)?;
    let address = sdp_origin_address(&options.local_ip);
    let addrtype = if address.contains(':') { "IP6" } else { "IP4" };
    let sess = sdp_session_id(options.now_ms);
    let mut out = String::new();
    for line in [
        "v=0",
        &format!("o=b2bua {sess} {sess} IN {addrtype} {address}"),
        "s=-",
        &format!("c=IN {addrtype} {address}"),
        "t=0 0",
    ] {
        push_line(&mut out, line, "\r\n");
    }
    for (stream, section) in doc.media.iter().zip(&sections.media) {
        let value = match stream.port {
            Some(0) => section.value().to_string(),
            _ => with_port_zero(section.value()),
        };
        push_line(&mut out, &format!("m={value}"), "\r\n");
        push_line(&mut out, "a=inactive", "\r\n");
    }
    Some(crate::sdp_octets::encode(&out))
}
