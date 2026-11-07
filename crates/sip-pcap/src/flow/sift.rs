//! Which captured datagrams the flow reads: a cheap SIP pre-filter, and the
//! key that tells one packet seen twice by the capture stack.

use crate::Datagram;

/// Cheap pre-filter so RTP/STUN/DNS on captured ports never reaches the parser.
pub(super) fn looks_like_sip(payload: &[u8]) -> bool {
    if payload.len() < 16 {
        return false;
    }
    let head = &payload[..payload.len().min(256)];
    (head.starts_with(b"SIP/2.0 ") || head.windows(9).any(|w| w == b" SIP/2.0\r"))
        && payload[0].is_ascii_uppercase()
}

pub(super) fn hash_datagram(d: &Datagram) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    d.src.hash(&mut h);
    d.dst.hash(&mut h);
    d.payload.hash(&mut h);
    h.finish()
}
