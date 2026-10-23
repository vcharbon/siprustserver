//! What one message says about the dialog it rides, read off its tags and
//! its header block: the peer's tag, the dialog URIs, the Record-Route stack.

use sip_message::header::Uri;
use sip_message::sniff::{self, UriFacts};
use sip_message::SipStr;

use crate::wire::{Kind, Msg};

/// The PEER's dialog tag on `msg` as `endpoint` sees it. A request names its
/// peer in the header the OTHER side owns: To when the endpoint sent it, From
/// when the endpoint took it; a response is the mirror. `""` where the peer has
/// not minted one yet.
pub(super) fn remote_tag_of(msg: &Msg, sent: bool) -> &str {
    let peer_is_to = match &msg.kind {
        Kind::Request { .. } => sent,
        Kind::Response { .. } => !sent,
    };
    let tag = if peer_is_to { msg.to_tag.as_deref() } else { msg.from_tag.as_deref() };
    tag.unwrap_or_default()
}

/// Fill `slot` from `msg`'s address header when it is still empty and the
/// header reads — the first establishing INVITE fixes the pair.
pub(super) fn set_if_empty(slot: &mut String, msg: &Msg, header: &str) {
    if !slot.is_empty() {
        return;
    }
    if let Some(uri) = msg.head.as_deref().and_then(|h| sniff::name_addr_uri(h, header)) {
        *slot = uri;
    }
}

/// The Record-Route stack `msg` carries, in wire order. A row no reader accepts
/// contributes nothing: an unreadable header is the grammar rules' finding. A
/// stack naming a hop no request can be addressed to is no route set at all
/// (RFC 3261 §12.2.1.1 builds the Request-URI from a strict hop), so the
/// dialog keeps none.
pub(super) fn record_route_set(msg: &Msg) -> Vec<UriFacts> {
    msg.head
        .as_deref()
        .and_then(|h| sniff::route_uris(h, "Record-Route"))
        .filter(|set| set.iter().all(|hop| hop.addressable))
        .unwrap_or_default()
}

/// The routing facts of the Request-URI in `head` when its URI names a host
/// RFC 3263 §4 resolves; `None` for a URI no reader accepts or one whose
/// scheme names no host (`urn:`, `http:` …, [`Uri::opaque_part`]).
pub(super) fn request_uri_target(head: &[u8]) -> Option<UriFacts> {
    let uri = Uri::parse(&SipStr::owned(&sniff::request_uri(head)?)).ok()?;
    uri.opaque_part().is_none().then(|| sniff::request_uri_facts(head)).flatten()
}
