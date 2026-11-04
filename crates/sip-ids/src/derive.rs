//! A tag derived from a per-call seed: any holder of the seed computes the
//! same tag for the same inputs, and nobody else can predict it.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::render;

/// The tag `seed` derives for `parts`: HMAC-SHA256 keyed by the seed over the
/// length-prefixed parts, rendered as a tag. A seed drawn from a keyed stream
/// ([`crate::TurnIds::draw`]) keeps the tag as unguessable as a drawn one
/// (RFC 3261 §19.3); two inputs that differ in any part, or in where one part
/// ends, derive unrelated tags.
pub fn derived_tag(seed: u64, parts: &[&[u8]]) -> String {
    let mut block = hmac::digest::Key::<Hmac<Sha256>>::default();
    for (slot, byte) in block.iter_mut().zip(seed.to_le_bytes()) {
        *slot = byte;
    }
    let mut mac = Hmac::<Sha256>::new(&block);
    for part in parts {
        mac.update(&(part.len() as u64).to_le_bytes());
        mac.update(part);
    }
    let mut first = [0u8; 8];
    for (slot, byte) in first.iter_mut().zip(mac.finalize().into_bytes()) {
        *slot = byte;
    }
    render::tag(u64::from_le_bytes(first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_seed_derives_one_tag_per_input() {
        assert_eq!(derived_tag(7, &[b"a", b"x"]), derived_tag(7, &[b"a", b"x"]));
        assert_ne!(derived_tag(7, &[b"a", b"x"]), derived_tag(8, &[b"a", b"x"]));
        assert_ne!(derived_tag(7, &[b"a", b"x"]), derived_tag(7, &[b"a", b"y"]));
    }

    #[test]
    fn where_a_part_ends_is_part_of_the_input() {
        assert_ne!(derived_tag(7, &[b"ab", b"c"]), derived_tag(7, &[b"a", b"bc"]));
    }

    #[test]
    fn a_derived_tag_renders_as_a_tag() {
        let tag = derived_tag(u64::MAX, &[b"leg"]);
        assert_eq!(tag.len(), 8);
        assert!(tag.bytes().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()), "{tag}");
    }
}
