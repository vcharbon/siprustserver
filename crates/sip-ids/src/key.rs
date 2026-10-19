//! [`IdKey`]: the secret an identifier stream is keyed by.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// HMAC-SHA256 keyed once; each draw clones it. The output of a draw reveals
/// neither the key nor any other draw.
#[derive(Clone)]
pub struct IdKey {
    prf: Hmac<Sha256>,
}

impl std::fmt::Debug for IdKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdKey").finish_non_exhaustive()
    }
}

impl IdKey {
    /// The PRF under `key`, at most the hash's 64-byte block (a longer key
    /// fails the build): a shorter key is zero-padded to it, as HMAC keys it
    /// (RFC 2104 §2).
    pub fn new<const N: usize>(key: &[u8; N]) -> Self {
        let () = FitsBlock::<N>::CHECKED;
        let mut block = hmac::digest::Key::<Hmac<Sha256>>::default();
        for (slot, byte) in block.iter_mut().zip(key) {
            *slot = *byte;
        }
        Self { prf: Hmac::<Sha256>::new(&block) }
    }

    /// Draw `n` of the key's counter stream: HMAC(key, n), its first 8 bytes.
    pub fn counter_draw(&self, n: u64) -> u64 {
        self.draw(&[&n.to_le_bytes()])
    }

    /// HMAC(key, the concatenation of `parts`), its first 8 bytes. The
    /// counter stream hashes 8 bytes and a turn's 16, so the two never share
    /// an input under one key.
    pub(crate) fn draw(&self, parts: &[&[u8]]) -> u64 {
        let mut mac = self.prf.clone();
        for part in parts {
            mac.update(part);
        }
        let mut first = [0u8; 8];
        for (slot, byte) in first.iter_mut().zip(mac.finalize().into_bytes()) {
            *slot = byte;
        }
        u64::from_le_bytes(first)
    }
}

/// A key of `N` bytes fits HMAC-SHA256's 64-byte block, where HMAC
/// zero-pads it (a longer one HMAC hashes instead); [`CHECKED`](Self::CHECKED)
/// is evaluated at compile time.
struct FitsBlock<const N: usize>;

impl<const N: usize> FitsBlock<N> {
    #[expect(clippy::disallowed_macros, reason = "checked at compile time")]
    const CHECKED: () = assert!(N <= 64, "a key longer than the block is hashed, not padded");
}
