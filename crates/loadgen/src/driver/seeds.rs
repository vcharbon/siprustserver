//! The driver's per-call randomness: the id-seed windows, the minted
//! correlation tokens, the mix pick's generator, and a caught panic's message.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide per-call id-seed source: a unique 100k-wide window per call so a
/// fresh binder (whose `Ids` restart at 1) never mints Call-IDs colliding with
/// another in-flight or prior call against the same stateful SUT.
pub(super) fn next_seed(base: u64) -> u64 {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    base.wrapping_add(SERIAL.fetch_add(1, Ordering::Relaxed).wrapping_mul(100_000)).max(1)
}

/// A random per-call correlation token, formatted as a valid SIP user-part.
pub(super) fn mint_token() -> String {
    format!("lg{}", uuid::Uuid::new_v4().simple())
}

pub(super) fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

pub(super) fn panic_msg(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}
