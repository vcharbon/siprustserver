//! Audio verdict: classify recorded PCM against the reference clips by
//! nearest-signature with a margin, guarded by an energy floor.
//!
//! Port of `../sipjsserver/src/test-harness/media/audio/classify.ts`.
//! "Nearest with a margin" (relative match), not an absolute distance
//! threshold, survives a transcoding leg's DSP — the matched clip just has to be
//! clearly closer than every other reference. The silence/energy guard fails
//! loudly when too little audio arrived, so a dead media path can't masquerade
//! as a weak match.

use std::collections::BTreeMap;

use super::clips::{reference_clips, ClipName};
use super::spectral::{clip_signature, cosine_distance, MfccOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Matched,
    Ambiguous,
    Silence,
    NoAudio,
}

#[derive(Debug, Clone)]
pub struct MediaVerdict {
    pub classification: Classification,
    /// Nearest reference, or `None` when silence/no-audio/ambiguous.
    pub matched: Option<ClipName>,
    /// Cosine distance to the matched reference (lower = closer).
    pub distance: f64,
    /// Distance gap to the runner-up — the margin actually achieved.
    pub margin: f64,
    /// Distance to every reference, for reporting.
    pub distances: BTreeMap<ClipName, f64>,
    /// RMS in [0,1] full-scale, for the silence diagnosis.
    pub rms: f64,
}

pub struct ClassifyOptions {
    /// Min distance gap between best and runner-up to accept a match.
    pub margin: f64,
    /// RMS floor (full-scale fraction) below which audio is "silence".
    pub rms_floor: f64,
    /// Min samples required before classifying at all.
    pub min_samples: usize,
    /// Override the reference set (defaults to all bundled clips).
    pub references: Option<BTreeMap<ClipName, Vec<i16>>>,
}

impl Default for ClassifyOptions {
    fn default() -> Self {
        Self {
            margin: 0.05,
            rms_floor: 0.01,
            min_samples: 800, // 100 ms @ 8 kHz
            references: None,
        }
    }
}

fn rms_full_scale(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let acc: f64 = pcm
        .iter()
        .map(|&s| {
            let v = s as f64 / 32768.0;
            v * v
        })
        .sum();
    (acc / pcm.len() as f64).sqrt()
}

fn reference_signatures(refs: &Option<BTreeMap<ClipName, Vec<i16>>>) -> BTreeMap<ClipName, Vec<f64>> {
    let opts = MfccOptions::default();
    let owned;
    let clips: &BTreeMap<ClipName, Vec<i16>> = match refs {
        Some(r) => r,
        None => {
            owned = reference_clips();
            &owned
        }
    };
    clips
        .iter()
        .filter_map(|(&name, pcm)| clip_signature(pcm, &opts).map(|s| (name, s)))
        .collect()
}

/// Classify a single recorded clip against the reference set.
pub fn classify(pcm: &[i16], opts: &ClassifyOptions) -> MediaVerdict {
    let rms = rms_full_scale(pcm);
    let empty = BTreeMap::new();

    if pcm.len() < opts.min_samples {
        return MediaVerdict {
            classification: Classification::NoAudio,
            matched: None,
            distance: f64::INFINITY,
            margin: 0.0,
            distances: empty,
            rms,
        };
    }
    if rms < opts.rms_floor {
        return MediaVerdict {
            classification: Classification::Silence,
            matched: None,
            distance: f64::INFINITY,
            margin: 0.0,
            distances: empty,
            rms,
        };
    }

    let sig = match clip_signature(pcm, &MfccOptions::default()) {
        Some(s) => s,
        None => {
            return MediaVerdict {
                classification: Classification::Silence,
                matched: None,
                distance: f64::INFINITY,
                margin: 0.0,
                distances: empty,
                rms,
            }
        }
    };

    let refs = reference_signatures(&opts.references);
    let mut distances = BTreeMap::new();
    let mut best: Option<ClipName> = None;
    let mut best_d = f64::INFINITY;
    let mut second_d = f64::INFINITY;
    for (&name, ref_sig) in &refs {
        let d = cosine_distance(&sig, ref_sig);
        distances.insert(name, d);
        if d < best_d {
            second_d = best_d;
            best_d = d;
            best = Some(name);
        } else if d < second_d {
            second_d = d;
        }
    }

    let gap = if second_d.is_infinite() {
        f64::INFINITY
    } else {
        second_d - best_d
    };
    match best {
        None => MediaVerdict {
            classification: Classification::NoAudio,
            matched: None,
            distance: f64::INFINITY,
            margin: 0.0,
            distances,
            rms,
        },
        Some(_) if gap < opts.margin => MediaVerdict {
            classification: Classification::Ambiguous,
            matched: None,
            distance: best_d,
            margin: gap,
            distances,
            rms,
        },
        Some(b) => MediaVerdict {
            classification: Classification::Matched,
            matched: Some(b),
            distance: best_d,
            margin: gap,
            distances,
            rms,
        },
    }
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub label: Option<ClipName>,
    pub start_ms: f64,
    pub end_ms: f64,
    pub classification: Classification,
}

pub struct SequenceOptions {
    pub classify: ClassifyOptions,
    pub window_ms: f64,
    pub hop_ms: f64,
    pub sample_rate: u32,
}
impl Default for SequenceOptions {
    fn default() -> Self {
        Self {
            classify: ClassifyOptions::default(),
            window_ms: 400.0,
            hop_ms: 200.0,
            sample_rate: 8000,
        }
    }
}

/// Classify a recording as an ordered sequence of segments — for "ringback then
/// voice" assertions on a single uninterrupted flow.
pub fn classify_sequence(pcm: &[i16], opts: &SequenceOptions) -> Vec<Segment> {
    let win = ((opts.window_ms / 1000.0) * opts.sample_rate as f64).round() as usize;
    let hop = ((opts.hop_ms / 1000.0) * opts.sample_rate as f64).round() as usize;
    let hop = hop.max(1);

    let mut labeled: Vec<Segment> = Vec::new();
    let mut start = 0usize;
    loop {
        if !(start + win <= pcm.len() || (start == 0 && !pcm.is_empty())) {
            break;
        }
        let end = (start + win).min(pcm.len());
        let win_opts = ClassifyOptions {
            margin: opts.classify.margin,
            rms_floor: opts.classify.rms_floor,
            min_samples: opts.classify.min_samples.min(win),
            references: opts.classify.references.clone(),
        };
        let v = classify(&pcm[start..end], &win_opts);
        labeled.push(Segment {
            label: v.matched,
            classification: v.classification,
            start_ms: (start as f64 / opts.sample_rate as f64) * 1000.0,
            end_ms: (end as f64 / opts.sample_rate as f64) * 1000.0,
        });
        if end >= pcm.len() {
            break;
        }
        start += hop;
    }

    // Collapse consecutive windows sharing a label + classification.
    let mut segments: Vec<Segment> = Vec::new();
    for w in labeled {
        match segments.last_mut() {
            Some(prev) if prev.label == w.label && prev.classification == w.classification => {
                prev.end_ms = w.end_ms;
            }
            _ => segments.push(w),
        }
    }
    segments
}

/// Does a classified recording match an expected ordered sequence of labels
/// (ignoring silence gaps)?
pub fn matches_sequence(segments: &[Segment], expected: &[ClipName]) -> bool {
    let seen: Vec<ClipName> = segments
        .iter()
        .filter(|s| s.classification == Classification::Matched)
        .filter_map(|s| s.label)
        .collect();
    // Collapse adjacent duplicates.
    let mut collapsed: Vec<ClipName> = Vec::new();
    for l in seen {
        if collapsed.last() != Some(&l) {
            collapsed.push(l);
        }
    }
    collapsed == expected
}
