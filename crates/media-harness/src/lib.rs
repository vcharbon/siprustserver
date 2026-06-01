//! Test-only media harness: deterministic reference clips, the spectral audio
//! classifier that proves media reached the right peer, the offer/answer
//! `negotiate_call` helper, and a paused-clock advance helper tuned to the media
//! (sub-ptime) timescale.
//!
//! Port of the TS media test support (`src/test-harness/media/` + the
//! `tests/media` helpers). Carries no production code (ADR-0004).

pub mod audio;
pub mod negotiate;

pub use audio::{
    classify, classify_sequence, matches_sequence, reference_clip, reference_clips, Classification,
    ClassifyOptions, ClipName, MediaVerdict, Segment, SequenceOptions, CLIP_NAMES, CLIP_SAMPLE_RATE,
};
pub use negotiate::{corrupt_connection_addr, negotiate_call, NegotiateOptions, NegotiatedCall};

/// Paused-clock test helpers for media.
pub mod testkit {
    use std::time::Duration;

    /// Advance the paused tokio clock at media (sub-ptime) granularity.
    ///
    /// A paced RTP sender sleeps one ptime (20 ms) between frames; advancing
    /// past many such deadlines in one big step starves the loop. So step in
    /// small chunks and `yield_now` between them, letting the sender, the
    /// simulated-net delivery tasks, and the inbound recorder all make progress
    /// each step. The media-timescale version of CLAUDE.md's "drive the protocol
    /// between advances."
    pub async fn advance_media(total: Duration) {
        let step = Duration::from_millis(10);
        let mut remaining = total;
        while !remaining.is_zero() {
            let s = remaining.min(step);
            tokio::time::advance(s).await;
            tokio::task::yield_now().await;
            remaining -= s;
        }
    }
}
