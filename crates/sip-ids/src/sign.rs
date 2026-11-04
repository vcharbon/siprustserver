//! [`SigningKey`]: the secret a node signs what it states about its own
//! messages with, so a peer sharing the key can trust it on a message that
//! comes back.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The bytes a signature keeps of the HMAC-SHA256 digest: 128 bits.
pub const SIGNATURE_BYTES: usize = 16;

/// The fewest bytes a key may hold: no shorter than the signature it makes
/// (RFC 2104 §3).
pub const MIN_KEY_BYTES: usize = 16;

/// The most bytes a key may hold: HMAC-SHA256's block, which HMAC zero-pads
/// to (a longer key would be hashed first).
pub const MAX_KEY_BYTES: usize = 64;

/// HMAC-SHA256 keyed once. Its `Debug` shows no key material.
#[derive(Clone)]
pub struct SigningKey {
    prf: Hmac<Sha256>,
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey").finish_non_exhaustive()
    }
}

impl SigningKey {
    /// The key `secret` holds, `None` for one shorter than [`MIN_KEY_BYTES`]
    /// or longer than [`MAX_KEY_BYTES`].
    pub fn new(secret: &[u8]) -> Option<Self> {
        if !(MIN_KEY_BYTES..=MAX_KEY_BYTES).contains(&secret.len()) {
            return None;
        }
        let mut block = hmac::digest::Key::<Hmac<Sha256>>::default();
        for (slot, byte) in block.iter_mut().zip(secret) {
            *slot = *byte;
        }
        Some(Self { prf: Hmac::<Sha256>::new(&block) })
    }

    /// The signature of `parts`, each length-prefixed so no two splits of one
    /// byte string sign alike.
    pub fn sign(&self, parts: &[&[u8]]) -> [u8; SIGNATURE_BYTES] {
        let mut mac = self.prf.clone();
        for part in parts {
            mac.update(&(part.len() as u64).to_le_bytes());
            mac.update(part);
        }
        let mut out = [0u8; SIGNATURE_BYTES];
        for (slot, byte) in out.iter_mut().zip(mac.finalize().into_bytes()) {
            *slot = byte;
        }
        out
    }

    /// Whether `signature` is [`sign`](Self::sign) of `parts`, compared in
    /// constant time; a signature of any other length never verifies.
    pub fn verify(&self, parts: &[&[u8]], signature: &[u8]) -> bool {
        if signature.len() != SIGNATURE_BYTES {
            return false;
        }
        let expected = self.sign(parts);
        let diff = expected.iter().zip(signature).fold(0u8, |acc, (a, b)| acc | (a ^ b));
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SigningKey {
        SigningKey::new(&[byte; 32]).expect("a 32-byte key")
    }

    #[test]
    fn a_key_out_of_its_bounds_is_refused() {
        assert!(SigningKey::new(&[1; MIN_KEY_BYTES - 1]).is_none());
        assert!(SigningKey::new(&[1; MAX_KEY_BYTES + 1]).is_none());
        assert!(SigningKey::new(&[1; MIN_KEY_BYTES]).is_some());
        assert!(SigningKey::new(&[1; MAX_KEY_BYTES]).is_some());
    }

    #[test]
    fn a_signature_verifies_under_its_key_only() {
        let sig = key(1).sign(&[b"leg", b"sip:bob@host"]);
        assert!(key(1).verify(&[b"leg", b"sip:bob@host"], &sig));
        assert!(!key(2).verify(&[b"leg", b"sip:bob@host"], &sig));
        assert!(!key(1).verify(&[b"leg", b"sip:eve@host"], &sig));
        assert!(!key(1).verify(&[b"le", b"gsip:bob@host"], &sig));
    }

    #[test]
    fn a_truncated_signature_never_verifies() {
        let sig = key(1).sign(&[b"x"]);
        assert!(!key(1).verify(&[b"x"], &sig[..SIGNATURE_BYTES - 1]));
        assert!(!key(1).verify(&[b"x"], &[]));
    }

    #[test]
    fn debug_shows_no_key() {
        assert_eq!(format!("{:?}", key(9)), "SigningKey { .. }");
    }
}
