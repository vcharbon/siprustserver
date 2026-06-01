//! Deterministic reference clips + the spectral (MFCC) audio classifier.

pub mod classify;
pub mod clips;
pub mod spectral;

pub use classify::{
    classify, classify_sequence, matches_sequence, Classification, ClassifyOptions, MediaVerdict,
    Segment, SequenceOptions,
};
pub use clips::{reference_clip, reference_clips, ClipName, CLIP_NAMES, CLIP_SAMPLE_RATE};
pub use spectral::{clip_signature, cosine_distance, mfcc, signature, MfccOptions};
