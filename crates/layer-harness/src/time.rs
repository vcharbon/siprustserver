//! Recording-clock seam.
//!
//! `RecordedAnomaly` / `Stamped` carry an `at_ms` timestamp used only for the
//! renderer's `(at_ms, seq)` sort — `seq` is the real ordering authority. The
//! TS source read this from Effect's `Clock` (virtual under `TestClock`).
//!
//! The **rendered** report timeline no longer flows through here: the
//! `Recorder`'s channels now stamp `at_ms` via an injected `sip_clock::Clock`
//! (monotonic-anchored), so under a paused tokio runtime the report timestamps
//! advance with `tokio::time::advance` — deterministic. See `recorder.rs`.
//!
//! This free function remains for the wall-clock stamps that are **not**
//! rendered as a timeline and are ordering-only: the decorators' anomaly
//! `at_ms` and the simulated fabric's undeliverable-packet stamp (whose
//! ordering authority is `seq`). None is on a logic path, so it does not
//! violate the "all logic-path time through tokio" rule in the migration
//! strategy. `UdpPacket::arrival_ms` is NOT one of them — it is a behavioural
//! input and rides the binding endpoint's `sip_clock::Clock`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Best-effort wall-clock epoch milliseconds for report ordering only.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
