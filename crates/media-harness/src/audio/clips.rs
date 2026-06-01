//! Reference audio clips for the media verdict: distinct synthetic "voices" for
//! alice / bob / charlie and a real-cadence ringback tone. Generated
//! deterministically (no RNG, no clock) so tests are hermetic and reproducible.
//!
//! Port of `../sipjsserver/src/test-harness/media/audio/clips.ts`. The voices use
//! source–filter synthesis (glottal impulse train → formant resonators) so each
//! has a genuinely different spectral envelope — the thing MFCC classification
//! keys on. Ringback is a pure 425 Hz EU tone, spectrally unmistakable against
//! the broadband voices.

use std::f64::consts::PI;

pub const CLIP_SAMPLE_RATE: u32 = 8000;
const CLIP_DURATION_MS: usize = 2000;
const CLIP_SAMPLES: usize = (CLIP_SAMPLE_RATE as usize * CLIP_DURATION_MS) / 1000;

/// A reference clip name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClipName {
    Alice,
    Bob,
    Charlie,
    Ringback,
}

impl ClipName {
    pub fn as_str(self) -> &'static str {
        match self {
            ClipName::Alice => "alice",
            ClipName::Bob => "bob",
            ClipName::Charlie => "charlie",
            ClipName::Ringback => "ringback",
        }
    }
}

impl std::fmt::Display for ClipName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub const CLIP_NAMES: [ClipName; 4] = [
    ClipName::Alice,
    ClipName::Bob,
    ClipName::Charlie,
    ClipName::Ringback,
];

struct Formant {
    freq: f64,
    bw: f64,
    gain: f64,
}

struct VoiceSpec {
    f0: f64,
    formants: Vec<Formant>,
    noise_seed: u32,
}

fn voice_spec(name: ClipName) -> VoiceSpec {
    match name {
        ClipName::Alice => VoiceSpec {
            f0: 200.0,
            formants: vec![
                Formant { freq: 800.0, bw: 80.0, gain: 1.0 },
                Formant { freq: 1300.0, bw: 90.0, gain: 0.6 },
                Formant { freq: 2700.0, bw: 120.0, gain: 0.3 },
            ],
            noise_seed: 0x1a2b,
        },
        ClipName::Bob => VoiceSpec {
            f0: 110.0,
            formants: vec![
                Formant { freq: 500.0, bw: 70.0, gain: 1.0 },
                Formant { freq: 1000.0, bw: 90.0, gain: 0.7 },
                Formant { freq: 2400.0, bw: 120.0, gain: 0.25 },
            ],
            noise_seed: 0x3c4d,
        },
        ClipName::Charlie => VoiceSpec {
            f0: 150.0,
            formants: vec![
                Formant { freq: 650.0, bw: 80.0, gain: 1.0 },
                Formant { freq: 1700.0, bw: 100.0, gain: 0.65 },
                Formant { freq: 3000.0, bw: 130.0, gain: 0.35 },
            ],
            noise_seed: 0x5e6f,
        },
        ClipName::Ringback => unreachable!("ringback is synthesised separately"),
    }
}

/// Deterministic LCG → low-level aspiration noise; keeps voices broadband.
struct Lcg {
    state: u32,
}
impl Lcg {
    fn new(seed: u32) -> Self {
        Self { state: seed }
    }
    fn next(&mut self) -> f64 {
        self.state = self
            .state
            .wrapping_mul(1664525)
            .wrapping_add(1013904223);
        self.state as f64 / 0xffff_ffffu32 as f64 - 0.5
    }
}

/// Slow syllable-like amplitude envelope (raised-cosine bursts).
fn syllable_envelope(n: usize, total: usize) -> f64 {
    let syllable_hz = 4.0; // ~4 syllables/sec
    let phase = (2.0 * PI * syllable_hz * n as f64) / CLIP_SAMPLE_RATE as f64;
    let burst = 0.5 - 0.5 * phase.cos();
    // gentle fade in/out over 50 ms to avoid edge clicks
    let fade = ((50.0 / 1000.0) * CLIP_SAMPLE_RATE as f64).round() as usize;
    let mut edge = 1.0;
    if n < fade {
        edge = n as f64 / fade as f64;
    } else if n > total - fade {
        edge = (total - n) as f64 / fade as f64;
    }
    (0.3 + 0.7 * burst) * edge.max(0.0)
}

fn normalize(buf: &[f64], peak: f64) -> Vec<i16> {
    let max = buf.iter().fold(0.0f64, |m, &v| m.max(v.abs()));
    let scale = if max > 0.0 { (peak * 32767.0) / max } else { 0.0 };
    buf.iter()
        .map(|&v| (v * scale).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

struct Resonator {
    a1: f64,
    a2: f64,
    gain: f64,
    y1: f64,
    y2: f64,
}

fn synth_voice(spec: &VoiceSpec) -> Vec<i16> {
    let mut out = vec![0.0f64; CLIP_SAMPLES];
    let period = CLIP_SAMPLE_RATE as f64 / spec.f0;
    let mut noise = Lcg::new(spec.noise_seed);

    // Each formant is a 2nd-order resonator: y[n] = x[n] + a1 y[n-1] - a2 y[n-2].
    let mut res: Vec<Resonator> = spec
        .formants
        .iter()
        .map(|f| {
            let r = (-PI * f.bw / CLIP_SAMPLE_RATE as f64).exp();
            let theta = (2.0 * PI * f.freq) / CLIP_SAMPLE_RATE as f64;
            Resonator {
                a1: 2.0 * r * theta.cos(),
                a2: r * r,
                gain: f.gain,
                y1: 0.0,
                y2: 0.0,
            }
        })
        .collect();

    let mut next_pulse = 0.0f64;
    // Indexed loop: each sample depends on the resonator state carried across
    // iterations, so a plain `iter_mut` doesn't express it more clearly.
    #[allow(clippy::needless_range_loop)]
    for n in 0..CLIP_SAMPLES {
        // glottal impulse train + a little breath noise as the source
        let mut x = 0.15 * noise.next();
        if n as f64 >= next_pulse {
            x += 1.0;
            next_pulse += period;
        }
        let mut sample = 0.0;
        for f in res.iter_mut() {
            let y = x + f.a1 * f.y1 - f.a2 * f.y2;
            f.y2 = f.y1;
            f.y1 = y;
            sample += f.gain * y;
        }
        out[n] = sample * syllable_envelope(n, CLIP_SAMPLES);
    }
    normalize(&out, 0.6)
}

/// EU ringback: 425 Hz tone, 1 s on / 1 s off cadence.
fn synth_ringback() -> Vec<i16> {
    let mut out = vec![0.0f64; CLIP_SAMPLES];
    let on_samples = CLIP_SAMPLE_RATE as usize; // 1 s on
    let cycle = 2 * CLIP_SAMPLE_RATE as usize; // 1 s on + 1 s off
    for (n, slot) in out.iter_mut().enumerate() {
        let in_tone = n % cycle < on_samples;
        *slot = if in_tone {
            (2.0 * PI * 425.0 * n as f64 / CLIP_SAMPLE_RATE as f64).sin()
        } else {
            0.0
        };
    }
    normalize(&out, 0.6)
}

/// The reference PCM for a clip (synthesized fresh; cheap for ≤2 s clips).
pub fn reference_clip(name: ClipName) -> Vec<i16> {
    match name {
        ClipName::Ringback => synth_ringback(),
        other => synth_voice(&voice_spec(other)),
    }
}

/// All reference clips keyed by name.
pub fn reference_clips() -> std::collections::BTreeMap<ClipName, Vec<i16>> {
    CLIP_NAMES
        .iter()
        .map(|&n| (n, reference_clip(n)))
        .collect()
}
