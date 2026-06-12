//! [`HmacKeyProvider`] — signs/verifies the routing cookie the proxy stamps on
//! the Record-Route URI. Port of `security/HmacKeyProvider.ts` (the `static`
//! impl only; the kubernetes-secret fs-watch variant is deferred — ADR-0009).
//!
//! - `sign(input)` MACs `input` with the **current** key (HMAC-SHA256) and
//!   returns `{ kid, mac }` (the full 32-byte digest; the LB truncates to 128
//!   bits before base64url-encoding into the cookie).
//! - `verify_truncated(input, kid, mac_prefix)` recomputes the digest under the
//!   named key (current OR previous, for the rotation overlap window) and
//!   compares the leading [`TRUNCATED_MAC_BYTES`] in constant time (`subtle`,
//!   the `timingSafeEqual` analogue). `false` on unknown kid, on ANY prefix
//!   length other than the canonical truncation, or on mismatch — the provider
//!   enforces the length so no caller can accidentally accept a 1-byte prefix
//!   (a ≤256-guess forgery).

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// RFC 2104 §3: keys shorter than the hash output should be avoided.
const MIN_KEY_BYTES: usize = 16;

/// Canonical cookie-MAC truncation: 128 bits. Signers truncate to this;
/// `verify_truncated` accepts EXACTLY this many bytes.
pub const TRUNCATED_MAC_BYTES: usize = 16;

type HmacSha256 = Hmac<Sha256>;

/// One HMAC key: an opaque `kid` carried on the wire + the secret bytes.
#[derive(Clone)]
pub struct HmacKey {
    pub id: String,
    pub bytes: Vec<u8>,
}

impl HmacKey {
    pub fn new(id: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self { id: id.into(), bytes: bytes.into() }
    }
}

/// Layer-build-time validation failure (non-empty kid + minimum key length).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("HMAC key provider config error: {0}")]
pub struct HmacKeyProviderConfigError(pub String);

/// Result of [`HmacKeyProvider::sign`].
pub struct HmacSignResult {
    pub kid: String,
    /// Full HMAC-SHA256 digest (32 bytes).
    pub mac: [u8; 32],
}

/// The signing/verifying seam (port of `HmacKeyProviderApi`). A trait so the
/// LB depends on the seam and a k8s-secret impl can drop in later.
pub trait HmacKeyProvider: Send + Sync {
    fn sign(&self, input: &[u8]) -> HmacSignResult;
    /// Verify a truncated MAC prefix under `kid` (current or previous key).
    fn verify_truncated(&self, input: &[u8], kid: &str, mac_prefix: &[u8]) -> bool;
}

fn mac_for(key: &HmacKey, input: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(&key.bytes).expect("HMAC accepts any key length");
    mac.update(input);
    mac.finalize().into_bytes().into()
}

fn validate_key(key: &HmacKey, label: &str) -> Result<(), HmacKeyProviderConfigError> {
    if key.id.is_empty() {
        return Err(HmacKeyProviderConfigError(format!("{label} key id must be non-empty")));
    }
    if key.bytes.len() < MIN_KEY_BYTES {
        return Err(HmacKeyProviderConfigError(format!(
            "{label} key must be at least {MIN_KEY_BYTES} bytes (got {})",
            key.bytes.len()
        )));
    }
    Ok(())
}

/// The static provider: an active key + an optional previous key accepted by
/// verify only (rotation overlap).
pub struct StaticHmacKeyProvider {
    current: HmacKey,
    previous: Option<HmacKey>,
}

impl StaticHmacKeyProvider {
    /// Validate and build. Mirrors `staticLayer`'s checks: non-empty kid, min
    /// length, and a previous key whose id differs from current.
    pub fn new(current: HmacKey, previous: Option<HmacKey>) -> Result<Self, HmacKeyProviderConfigError> {
        validate_key(&current, "current")?;
        if let Some(prev) = &previous {
            validate_key(prev, "previous")?;
            if prev.id == current.id {
                return Err(HmacKeyProviderConfigError(format!(
                    "previous key id must differ from current key id (both are \"{}\")",
                    current.id
                )));
            }
        }
        Ok(Self { current, previous })
    }

    fn lookup_key(&self, kid: &str) -> Option<&HmacKey> {
        if kid == self.current.id {
            Some(&self.current)
        } else {
            self.previous.as_ref().filter(|p| p.id == kid)
        }
    }
}

impl HmacKeyProvider for StaticHmacKeyProvider {
    fn sign(&self, input: &[u8]) -> HmacSignResult {
        HmacSignResult { kid: self.current.id.clone(), mac: mac_for(&self.current, input) }
    }

    fn verify_truncated(&self, input: &[u8], kid: &str, mac_prefix: &[u8]) -> bool {
        let Some(key) = self.lookup_key(kid) else {
            return false;
        };
        // Exactly the canonical truncation — a shorter prefix would shrink the
        // forgery space (1 byte = a 256-guess cookie), and only the caller's
        // own length check used to stand in the way.
        if mac_prefix.len() != TRUNCATED_MAC_BYTES {
            return false;
        }
        let full = mac_for(key, input);
        full[..TRUNCATED_MAC_BYTES].ct_eq(mac_prefix).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> StaticHmacKeyProvider {
        StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap()
    }

    #[test]
    fn rejects_short_or_empty_keys() {
        assert!(StaticHmacKeyProvider::new(HmacKey::new("", vec![0u8; 32]), None).is_err());
        assert!(StaticHmacKeyProvider::new(HmacKey::new("k", vec![0u8; 4]), None).is_err());
        assert!(StaticHmacKeyProvider::new(
            HmacKey::new("k", vec![0u8; 32]),
            Some(HmacKey::new("k", vec![1u8; 32]))
        )
        .is_err());
    }

    #[test]
    fn short_prefix_is_rejected_even_if_it_matches() {
        // A 1-byte "prefix" of the genuine MAC must NOT verify: accepting it
        // would shrink the cookie's forgery space to 256 guesses. Only the
        // exact canonical truncation is acceptable.
        let p = provider();
        let signed = p.sign(b"input");
        for n in [0usize, 1, 8, 15, 17, 32] {
            assert!(
                !p.verify_truncated(b"input", "k1", &signed.mac[..n]),
                "a {n}-byte prefix must be rejected (only {TRUNCATED_MAC_BYTES} is valid)"
            );
        }
        assert!(p.verify_truncated(b"input", "k1", &signed.mac[..TRUNCATED_MAC_BYTES]));
    }

    #[test]
    fn sign_then_verify_truncated_roundtrips() {
        let p = provider();
        let signed = p.sign(b"v=3|w_pri=a|w_bak=b|e=0|c=call");
        assert_eq!(signed.kid, "k1");
        assert!(p.verify_truncated(b"v=3|w_pri=a|w_bak=b|e=0|c=call", "k1", &signed.mac[..16]));
    }

    #[test]
    fn tampered_input_or_mac_fails() {
        let p = provider();
        let signed = p.sign(b"input-A");
        assert!(!p.verify_truncated(b"input-B", "k1", &signed.mac[..16]));
        let mut bad = signed.mac;
        bad[0] ^= 0x01;
        assert!(!p.verify_truncated(b"input-A", "k1", &bad[..16]));
    }

    #[test]
    fn unknown_kid_and_bad_lengths_fail() {
        let p = provider();
        let signed = p.sign(b"x");
        assert!(!p.verify_truncated(b"x", "nope", &signed.mac[..16]));
        assert!(!p.verify_truncated(b"x", "k1", &[]));
        assert!(!p.verify_truncated(b"x", "k1", &[0u8; 33]));
    }

    #[test]
    fn previous_key_accepted_by_verify() {
        let p = StaticHmacKeyProvider::new(
            HmacKey::new("k2", vec![9u8; 32]),
            Some(HmacKey::new("k1", vec![7u8; 32])),
        )
        .unwrap();
        // A MAC minted under the old key (k1) still verifies during overlap.
        let old = StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap();
        let signed = old.sign(b"rotated");
        assert!(p.verify_truncated(b"rotated", "k1", &signed.mac[..16]));
        // The current key signs as k2.
        assert_eq!(p.sign(b"rotated").kid, "k2");
    }
}
