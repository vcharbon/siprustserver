//! Test-only media harness: deterministic reference clips, the spectral audio
//! classifier that proves media reached the right peer, the offer/answer
//! `negotiate_call` helper. A media test moves the paused clock with
//! `sip_clock::testkit::advance_settled`, which runs a paced sender at each of
//! its ptime deadlines.
//!
//! Port of the TS media test support (`src/test-harness/media/` + the
//! `tests/media` helpers). Carries no production code (ADR-0004).

pub mod audio;
pub mod negotiate;

pub use audio::{
    classify, classify_sequence, matches_sequence, reference_clip, reference_clips, Classification,
    ClassifyOptions, ClipName, MediaVerdict, Segment, SequenceOptions, CLIP_NAMES,
    CLIP_SAMPLE_RATE,
};
pub use negotiate::{corrupt_connection_addr, negotiate_call, NegotiateOptions, NegotiatedCall};
