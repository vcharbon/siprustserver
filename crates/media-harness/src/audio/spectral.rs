//! Spectral feature extraction for audio classification: framed MFCCs reduced
//! to a single clip-level signature, plus cosine distance between signatures.
//!
//! Port of `../sipjsserver/src/test-harness/media/audio/spectral.ts`. The
//! pipeline is the textbook one — pre-emphasis → Hamming-windowed frames → power
//! spectrum (FFT via `rustfft`) → mel filterbank → log → DCT-II → keep the low
//! cepstral coefficients. The clip signature is the per-frame MFCC mean over
//! voiced frames (dropping c0 so the match is gain-invariant), which makes the
//! nearest-reference verdict robust to the level changes and companding a G.711
//! leg introduces.

use std::sync::Arc;

use rustfft::{num_complex::Complex, Fft, FftPlanner};

const DEFAULT_SAMPLE_RATE: u32 = 8000;
const FRAME_MS: f64 = 25.0;
const HOP_MS: f64 = 10.0;
const PRE_EMPHASIS: f64 = 0.97;
const MEL_FILTERS: usize = 26;
const MFCC_COEFFS: usize = 13; // includes c0; the signature drops c0
const FFT_SIZE: usize = 256; // next pow2 ≥ 25 ms @ 8 kHz (200 samples)

fn hz_to_mel(hz: f64) -> f64 {
    2595.0 * (1.0 + hz / 700.0).log10()
}
fn mel_to_hz(mel: f64) -> f64 {
    700.0 * (10f64.powf(mel / 2595.0) - 1.0)
}

/// Build the triangular mel filterbank (filter index → per-bin weights).
fn mel_filterbank(sample_rate: u32, fft_size: usize, num_filters: usize) -> Vec<Vec<f64>> {
    let nyquist = sample_rate as f64 / 2.0;
    let mel_max = hz_to_mel(nyquist);
    let points: Vec<usize> = (0..num_filters + 2)
        .map(|i| {
            let hz = mel_to_hz((mel_max * i as f64) / (num_filters as f64 + 1.0));
            (((fft_size + 1) as f64 * hz) / sample_rate as f64).floor() as usize
        })
        .collect();
    let bins = fft_size / 2 + 1;
    let mut filters = Vec::with_capacity(num_filters);
    for m in 1..=num_filters {
        let mut f = vec![0.0f64; bins];
        let left = points[m - 1];
        let center = points[m];
        let right = points[m + 1];
        for (k, slot) in f.iter_mut().enumerate().take(center).skip(left) {
            if center > left {
                *slot = (k - left) as f64 / (center - left) as f64;
            }
        }
        for (k, slot) in f.iter_mut().enumerate().take(right).skip(center) {
            if right > center {
                *slot = (right - k) as f64 / (right - center) as f64;
            }
        }
        filters.push(f);
    }
    filters
}

fn hamming_window(size: usize) -> Vec<f64> {
    (0..size)
        .map(|i| 0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (size as f64 - 1.0)).cos())
        .collect()
}

/// DCT-II of `input`, keeping the first `coeffs` outputs.
fn dct(input: &[f64], coeffs: usize) -> Vec<f64> {
    let n = input.len();
    (0..coeffs)
        .map(|k| {
            input
                .iter()
                .enumerate()
                .map(|(i, &v)| v * (std::f64::consts::PI * k as f64 * (i as f64 + 0.5) / n as f64).cos())
                .sum()
        })
        .collect()
}

/// Per-frame MFCC vectors plus per-frame log-energy (for the silence guard).
pub struct MfccFrames {
    pub frames: Vec<Vec<f64>>,
    pub log_energy: Vec<f64>,
}

pub struct MfccOptions {
    pub sample_rate: u32,
    pub voiced_floor_db: f64,
}
impl Default for MfccOptions {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            voiced_floor_db: 25.0,
        }
    }
}

/// Compute per-frame MFCCs over a PCM clip.
pub fn mfcc(pcm: &[i16], sample_rate: u32) -> MfccFrames {
    let frame_len = ((FRAME_MS / 1000.0) * sample_rate as f64).round() as usize;
    let hop = ((HOP_MS / 1000.0) * sample_rate as f64).round() as usize;
    let window = hamming_window(frame_len);
    let filters = mel_filterbank(sample_rate, FFT_SIZE, MEL_FILTERS);
    let bins = FFT_SIZE / 2 + 1;

    let mut planner = FftPlanner::<f64>::new();
    let fft: Arc<dyn Fft<f64>> = planner.plan_fft_forward(FFT_SIZE);

    let mut frames = Vec::new();
    let mut log_energy = Vec::new();

    if pcm.is_empty() || frame_len == 0 || hop == 0 {
        return MfccFrames { frames, log_energy };
    }

    let mut start = 0usize;
    while start + frame_len <= pcm.len() {
        let mut buf = vec![Complex::<f64>::new(0.0, 0.0); FFT_SIZE];
        let mut energy = 0.0;
        let mut prev = if start > 0 { pcm[start - 1] } else { pcm[start] } as f64;
        for i in 0..frame_len {
            let sample = pcm[start + i] as f64;
            let emphasized = sample - PRE_EMPHASIS * prev;
            prev = sample;
            let w = emphasized * window[i];
            buf[i].re = w;
            energy += w * w;
        }
        log_energy.push((energy + 1e-10).ln());

        fft.process(&mut buf);
        let mut power = vec![0.0f64; bins];
        for (k, p) in power.iter_mut().enumerate() {
            *p = (buf[k].re * buf[k].re + buf[k].im * buf[k].im) / FFT_SIZE as f64;
        }
        let mut mel_log = vec![0.0f64; MEL_FILTERS];
        for (m, slot) in mel_log.iter_mut().enumerate() {
            let filt = &filters[m];
            let acc: f64 = power.iter().zip(filt).map(|(p, w)| p * w).sum();
            *slot = (acc + 1e-10).ln();
        }
        frames.push(dct(&mel_log, MFCC_COEFFS));

        start += hop;
    }

    MfccFrames { frames, log_energy }
}

/// Reduce per-frame MFCCs to one clip-level signature: the mean of c1..cN over
/// voiced frames (c0 dropped → gain-invariant). Returns `None` if no voiced frame.
pub fn signature(frames: &MfccFrames, voiced_floor_db: f64) -> Option<Vec<f64>> {
    if frames.frames.is_empty() {
        return None;
    }
    let max_log = frames
        .log_energy
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    // dB floor relative to the loudest frame (10*log10 over natural-log energy).
    let floor = max_log - (voiced_floor_db / 10.0) * std::f64::consts::LN_10;

    let dim = MFCC_COEFFS - 1;
    let mut sum = vec![0.0f64; dim];
    let mut count = 0usize;
    for (i, v) in frames.frames.iter().enumerate() {
        if frames.log_energy[i] < floor {
            continue;
        }
        for d in 0..dim {
            sum[d] += v[d + 1];
        }
        count += 1;
    }
    if count == 0 {
        return None;
    }
    for s in sum.iter_mut() {
        *s /= count as f64;
    }
    Some(sum)
}

/// Cosine distance (1 - cosine similarity) in [0, 2].
pub fn cosine_distance(a: &[f64], b: &[f64]) -> f64 {
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 1.0;
    }
    1.0 - dot / (na.sqrt() * nb.sqrt())
}

/// Convenience: signature of a raw PCM clip in one call.
pub fn clip_signature(pcm: &[i16], opts: &MfccOptions) -> Option<Vec<f64>> {
    signature(&mfcc(pcm, opts.sample_rate), opts.voiced_floor_db)
}
