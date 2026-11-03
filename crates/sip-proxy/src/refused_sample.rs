//! A debug sample of the datagrams the SIP parser refused: the source, the
//! parse error and the datagram's first bytes, at most [`SAMPLES_PER_SEC`] a
//! second process-wide, so a malformed flood cannot flood the log. Enabled
//! with `RUST_LOG=…,sip_proxy::refused_sample=debug`; otherwise it costs one
//! level check per refused datagram.

use std::fmt::Display;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Samples logged per second at most, over every core of the process.
pub const SAMPLES_PER_SEC: u32 = 5;
/// The datagram's leading bytes a sample shows, escaped.
pub const HEAD_BYTES: usize = 160;

/// The second the current budget belongs to, and how much of it is spent.
static WINDOW_S: AtomicU64 = AtomicU64::new(u64::MAX);
static SPENT: AtomicU32 = AtomicU32::new(0);

/// Log the refused datagram `raw` from `src` at `now_ms`, when debug is on for
/// this target and the second's budget is not spent. The budget is approximate
/// under concurrent cores: a sample, not a count (the count is
/// `sip_proxy_parse_errors_total`).
pub(crate) fn sample(src: SocketAddr, error: &impl Display, raw: &[u8], now_ms: u64) {
    if !tracing::enabled!(tracing::Level::DEBUG) || !admit(now_ms / 1000) {
        return;
    }
    let head = raw.get(..HEAD_BYTES).unwrap_or(raw);
    tracing::debug!(
        %src,
        len = raw.len(),
        %error,
        head = %head.escape_ascii(),
        "datagram refused by the SIP parser"
    );
}

/// Whether one more sample fits the budget of second `now_s`.
fn admit(now_s: u64) -> bool {
    if WINDOW_S.swap(now_s, Ordering::Relaxed) != now_s {
        SPENT.store(0, Ordering::Relaxed);
    }
    SPENT.fetch_add(1, Ordering::Relaxed) < SAMPLES_PER_SEC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_admits_its_budget_then_the_next_second_admits_again() {
        let admitted = (0..20).filter(|_| admit(7)).count();
        assert_eq!(admitted, SAMPLES_PER_SEC as usize);
        assert!(admit(8), "a new second has a fresh budget");
    }
}
