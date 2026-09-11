//! Correlation-id minting for per-call traces (ADR-0026).
//!
//! Ids are W3C-shaped — a 32-hex trace id, a 16-hex span id — and are minted
//! HERE rather than by the exporter, so a domain crate populates
//! `Call.trace_id` / `Call.root_span_id` and links a takeover span without
//! linking the OpenTelemetry dependency tree. They ride on every span as
//! attributes, which is also what the HA link reads.
//!
//! The stream is a per-process xorshift64* mixed with a monotonic counter: ids
//! are unique within a process and unguessable enough for correlation. It is
//! not a cryptographic source.

use std::sync::atomic::{AtomicU64, Ordering};

/// Hex length of a trace id.
pub const TRACE_ID_HEX: usize = 32;

/// Hex length of a span id.
pub const SPAN_ID_HEX: usize = 16;

static COUNTER: AtomicU64 = AtomicU64::new(0);
static SEED: AtomicU64 = AtomicU64::new(0);

/// A fresh trace id: 32 lowercase hex digits, never all-zero.
pub fn new_trace_id() -> String {
    format!("{:016x}{:016x}", next_nonzero(), next_nonzero())
}

/// A fresh span id: 16 lowercase hex digits, never all-zero.
pub fn new_span_id() -> String {
    format!("{:016x}", next_nonzero())
}

/// Whether `id` is a syntactically usable correlation id of `width` hex digits.
/// A replicated id that fails this is not linked against — a malformed link
/// target is worse than none.
pub fn is_valid_id(id: &str, width: usize) -> bool {
    id.len() == width && id.bytes().all(|b| b.is_ascii_hexdigit()) && id.bytes().any(|b| b != b'0')
}

fn next_nonzero() -> u64 {
    let mut x =
        seed() ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    if x == 0 {
        1
    } else {
        x
    }
}

/// The process seed, drawn once from wall-clock + pid entropy. Read at first
/// use, never on a paused-clock behaviour path (id minting happens only for an
/// admitted call, and admission is inert without an exporter).
fn seed() -> u64 {
    let current = SEED.load(Ordering::Relaxed);
    if current != 0 {
        return current;
    }
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5DEE_CE66);
    let fresh = (t ^ ((std::process::id() as u64) << 32)) | 1;
    SEED.store(fresh, Ordering::Relaxed);
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_have_the_w3c_widths_and_are_valid() {
        let trace = new_trace_id();
        let span = new_span_id();
        assert_eq!(trace.len(), TRACE_ID_HEX);
        assert_eq!(span.len(), SPAN_ID_HEX);
        assert!(is_valid_id(&trace, TRACE_ID_HEX));
        assert!(is_valid_id(&span, SPAN_ID_HEX));
    }

    #[test]
    fn ids_do_not_repeat_within_a_process() {
        let ids: HashSet<String> = (0..10_000).map(|_| new_span_id()).collect();
        assert_eq!(ids.len(), 10_000, "span ids collided");
    }

    #[test]
    fn a_malformed_or_zero_id_is_refused() {
        assert!(!is_valid_id("", TRACE_ID_HEX));
        assert!(!is_valid_id(&"0".repeat(TRACE_ID_HEX), TRACE_ID_HEX));
        assert!(!is_valid_id(&"z".repeat(TRACE_ID_HEX), TRACE_ID_HEX));
        assert!(!is_valid_id(&"a".repeat(TRACE_ID_HEX - 1), TRACE_ID_HEX));
    }
}
