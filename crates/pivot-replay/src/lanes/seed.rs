//! The seed a cell's ids derive from. A hosted lane's cell takes its batch's
//! seed and its ordinal in the batch, unique across the cells a standing SUT
//! remembers; a simulated-lane cell, alone with its SUT, takes its case id's seed,
//! so two runs of one document mint the same ids.

use sha1::{Digest, Sha1};

/// The dialog-identity nonce of the cell at `ordinal` of the batch seeded
/// `seed`: unique across batches through the seed, across cells through the
/// ordinal (the seed at a fixed width, so no two pairs read alike).
pub fn cell_nonce(seed: u64, ordinal: u64) -> String {
    format!("{seed:016x}{ordinal:x}")
}

/// The seed `text` names: the first eight bytes of its SHA-1, never zero.
pub fn seed_of(text: &str) -> u64 {
    let digest = Sha1::digest(text.as_bytes());
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head).max(1)
}

#[cfg(test)]
mod tests;
