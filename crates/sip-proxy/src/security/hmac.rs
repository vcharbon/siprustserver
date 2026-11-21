//! [`HmacKeyProvider`]: signs and verifies the routing cookie the proxy stamps
//! on its Record-Route URI and Via (ADR-0009 X3), and its static implementation.
//!
//! - `sign(input)` MACs `input` with the current key (HMAC-SHA256) and returns
//!   `{ kid, mac }`, the full 32-byte digest; the load balancer truncates it to
//!   [`TRUNCATED_MAC_BYTES`] before base64url-encoding it into the cookie.
//! - `verify_truncated(input, kid, mac_prefix)` recomputes the digest under the
//!   named key (the current one, or the previous one during a rotation overlap)
//!   and compares its leading [`TRUNCATED_MAC_BYTES`] in constant time. It
//!   returns `false` on an unknown kid, on any prefix length other than
//!   [`TRUNCATED_MAC_BYTES`], or on a mismatch: the provider enforces the
//!   length, so no caller accepts a short prefix (a 1-byte one is 256 guesses
//!   from a forgery).

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sip_message::char_class::{is_paramchar, is_run_of};
use subtle::ConstantTimeEq;

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

pub struct HmacSignResult {
    pub kid: String,
    /// Full HMAC-SHA256 digest (32 bytes).
    pub mac: [u8; 32],
}

/// The seam that signs and verifies the routing cookie's MAC.
pub trait HmacKeyProvider: Send + Sync {
    fn sign(&self, input: &[u8]) -> HmacSignResult;
    /// Verify a truncated MAC prefix under `kid` (current or previous key).
    fn verify_truncated(&self, input: &[u8], kid: &str, mac_prefix: &[u8]) -> bool;
}

/// A key ready to MAC with: its `kid` and the HMAC state keyed once, at
/// construction.
struct Keyed {
    id: String,
    prf: HmacSha256,
}

impl Keyed {
    /// `key`, validated and keyed; `label` names it in the error.
    fn of(key: &HmacKey, label: &str) -> Result<Self, HmacKeyProviderConfigError> {
        validate_key(key, label)?;
        let prf = HmacSha256::new_from_slice(&key.bytes)
            .map_err(|e| HmacKeyProviderConfigError(format!("{label} key: {e}")))?;
        Ok(Self { id: key.id.clone(), prf })
    }

    fn mac(&self, input: &[u8]) -> [u8; 32] {
        let mut mac = self.prf.clone();
        mac.update(input);
        mac.finalize().into_bytes().into()
    }
}

/// A key's id rides the Record-Route cookie unescaped, as a URI parameter value
/// (`pvalue = 1*paramchar`, RFC 3261 §25.1): it must be a non-empty run of
/// paramchars, and the key at least [`MIN_KEY_BYTES`] long.
fn validate_key(key: &HmacKey, label: &str) -> Result<(), HmacKeyProviderConfigError> {
    if key.id.is_empty() {
        return Err(HmacKeyProviderConfigError(format!("{label} key id must be non-empty")));
    }
    if !is_run_of(&key.id, is_paramchar, false) {
        return Err(HmacKeyProviderConfigError(format!(
            "{label} key id {:?} must be URI-parameter characters (RFC 3261 §25.1 paramchar)",
            key.id
        )));
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
    current: Keyed,
    previous: Option<Keyed>,
}

impl StaticHmacKeyProvider {
    /// Validate and build: each key id a non-empty run of URI-parameter
    /// characters, each key at least 16 bytes long (`MIN_KEY_BYTES`), and the
    /// previous key's id other than the current one's.
    pub fn new(
        current: HmacKey,
        previous: Option<HmacKey>,
    ) -> Result<Self, HmacKeyProviderConfigError> {
        let current = Keyed::of(&current, "current")?;
        let previous = previous.map(|prev| Keyed::of(&prev, "previous")).transpose()?;
        if previous.as_ref().is_some_and(|prev| prev.id == current.id) {
            return Err(HmacKeyProviderConfigError(format!(
                "previous key id must differ from current key id (both are \"{}\")",
                current.id
            )));
        }
        Ok(Self { current, previous })
    }

    fn lookup_key(&self, kid: &str) -> Option<&Keyed> {
        if kid == self.current.id {
            Some(&self.current)
        } else {
            self.previous.as_ref().filter(|p| p.id == kid)
        }
    }
}

impl HmacKeyProvider for StaticHmacKeyProvider {
    fn sign(&self, input: &[u8]) -> HmacSignResult {
        HmacSignResult { kid: self.current.id.clone(), mac: self.current.mac(input) }
    }

    fn verify_truncated(&self, input: &[u8], kid: &str, mac_prefix: &[u8]) -> bool {
        let Some(key) = self.lookup_key(kid) else {
            return false;
        };
        if mac_prefix.len() != TRUNCATED_MAC_BYTES {
            return false;
        }
        let full = key.mac(input);
        full[..TRUNCATED_MAC_BYTES].ct_eq(mac_prefix).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> StaticHmacKeyProvider {
        StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap()
    }

    /// A key id the Record-Route cookie could not carry as a URI parameter
    /// value is refused at load, the previous key's too.
    #[test]
    fn rejects_a_key_id_no_uri_parameter_carries() {
        for kid in ["k@1", "k 1", "k\"1", "k%1", "k;1", "k=1", "k?1", "kë"] {
            let key = HmacKey::new(kid, vec![0u8; 32]);
            assert!(StaticHmacKeyProvider::new(key.clone(), None).is_err(), "{kid:?}");
            let current = HmacKey::new("k1", vec![0u8; 32]);
            assert!(StaticHmacKeyProvider::new(current, Some(key)).is_err(), "previous {kid:?}");
        }
        assert!(
            StaticHmacKeyProvider::new(HmacKey::new("k-1_2.v[3]:$", vec![0u8; 32]), None).is_ok()
        );
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

    /// A prefix of the genuine MAC of any length but the canonical truncation
    /// never verifies: a 1-byte prefix would leave the cookie 256 guesses from a
    /// forgery.
    #[test]
    fn short_prefix_is_rejected_even_if_it_matches() {
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
        let old = StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap();
        let signed = old.sign(b"rotated");
        assert!(p.verify_truncated(b"rotated", "k1", &signed.mac[..16]));
        assert_eq!(p.sign(b"rotated").kid, "k2");
    }
}
