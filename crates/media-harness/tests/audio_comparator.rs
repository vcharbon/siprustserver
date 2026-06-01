//! Slice 0: the audio comparator. Clips self-classify, cross-separation holds,
//! the silence/energy guard fires, G.711 transcoding survives classification,
//! and a ringback→voice concatenation classifies as the ordered sequence.
//!
//! Port of `../sipjsserver/tests/media/audio-comparator.test.ts`.

use media::codec::{g711_round_trip, G711Codec};
use media_harness::audio::spectral::{clip_signature, cosine_distance, MfccOptions};
use media_harness::{
    classify, classify_sequence, matches_sequence, reference_clip, reference_clips, Classification,
    ClassifyOptions, ClipName, SequenceOptions, CLIP_NAMES, CLIP_SAMPLE_RATE,
};

const VOICES: [ClipName; 3] = [ClipName::Alice, ClipName::Bob, ClipName::Charlie];

#[test]
fn each_clip_classifies_as_itself_with_a_margin() {
    for name in CLIP_NAMES {
        let v = classify(&reference_clip(name), &ClassifyOptions::default());
        assert_eq!(v.classification, Classification::Matched, "{name}");
        assert_eq!(v.matched, Some(name), "{name}");
        assert!(v.margin > 0.05, "{name} margin {}", v.margin);
    }
}

#[test]
fn every_clips_nearest_reference_is_itself() {
    let clips = reference_clips();
    let opts = MfccOptions::default();
    let sigs: Vec<(ClipName, Vec<f64>)> = clips
        .iter()
        .map(|(&n, pcm)| (n, clip_signature(pcm, &opts).expect("signature")))
        .collect();
    for (a, sig_a) in &sigs {
        let self_d = cosine_distance(sig_a, sig_a);
        for (b, sig_b) in &sigs {
            if a == b {
                continue;
            }
            let cross = cosine_distance(sig_a, sig_b);
            assert!(cross > self_d + 0.05, "{a} vs {b}: cross {cross} self {self_d}");
        }
    }
}

#[test]
fn empty_pcm_is_no_audio() {
    let v = classify(&[], &ClassifyOptions::default());
    assert_eq!(v.classification, Classification::NoAudio);
    assert_eq!(v.matched, None);
}

#[test]
fn near_silent_pcm_is_silence() {
    // ±1 LSB dither over 1 s — below the RMS floor.
    let quiet: Vec<i16> = (0..CLIP_SAMPLE_RATE as usize)
        .map(|i| (i % 3) as i16 - 1)
        .collect();
    let v = classify(&quiet, &ClassifyOptions::default());
    assert_eq!(v.classification, Classification::Silence);
    assert_eq!(v.matched, None);
}

#[test]
fn clips_survive_g711_round_trip() {
    for codec in [G711Codec::Pcma, G711Codec::Pcmu] {
        for name in VOICES {
            let transcoded = g711_round_trip(&reference_clip(name), codec);
            let v = classify(&transcoded, &ClassifyOptions::default());
            assert_eq!(v.classification, Classification::Matched, "{name} {codec:?}");
            assert_eq!(v.matched, Some(name), "{name} {codec:?}");
        }
    }
}

#[test]
fn ringback_then_voice_classifies_as_ordered_sequence() {
    let rbt = reference_clip(ClipName::Ringback);
    let bob = reference_clip(ClipName::Bob);
    let mut joined = rbt.clone();
    joined.extend_from_slice(&bob);

    let segments = classify_sequence(
        &joined,
        &SequenceOptions {
            sample_rate: CLIP_SAMPLE_RATE,
            ..Default::default()
        },
    );
    assert!(matches_sequence(&segments, &[ClipName::Ringback, ClipName::Bob]));
    assert!(!matches_sequence(&segments, &[ClipName::Bob, ClipName::Ringback]));
}
