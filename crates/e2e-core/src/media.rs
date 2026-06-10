//! Media folding (ADR-0018 Phase J): a media-exchanging Callflow shape
//! deposits per-agent [`MediaCapture`]s on the [`InfraRuntime`](crate::infra::InfraRuntime);
//! this module classifies each recording (the `media-harness` spectral MFCC
//! classifier), writes it as a sibling `<agent>.received.wav` (PCM16 mono
//! 8 kHz — never inlined in `result.json`), and folds the verdict into a
//! check (`<agent>.media hears <clip>`), exactly like any other check
//! (ADR-0019: the classifier verdict is just another check).

use std::io;
use std::path::Path;

use media_harness::{Classification, ClassifyOptions, ClipName, classify};

use crate::checks::CheckVerdict;
use crate::model::CheckOp;
use crate::result::MediaRef;

/// What one agent RECEIVED during the call, plus the clip its peer streamed
/// (the expectation the classifier verdict is judged against).
#[derive(Debug, Clone)]
pub struct MediaCapture {
    pub agent: String,
    /// The reference clip the agent is expected to hear.
    pub expected: ClipName,
    /// Recorded inbound PCM, i16 @ 8 kHz (`MediaSession::recorded().pcm`).
    pub pcm: Vec<i16>,
}

/// Minimal RIFF/WAVE container for PCM16 mono 8 kHz.
pub fn wav_bytes(pcm: &[i16]) -> Vec<u8> {
    const RATE: u32 = 8_000;
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // audio format: PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels: mono
    out.extend_from_slice(&RATE.to_le_bytes()); // sample rate
    out.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Classify each capture, write its `.wav` beside the (future) `result.json`
/// in `cell_dir`, and return the artifact refs + the "hears" check verdicts.
/// No captures → nothing written, both vectors empty.
pub fn write_and_fold(
    cell_dir: &Path,
    captures: &[MediaCapture],
) -> io::Result<(Vec<MediaRef>, Vec<CheckVerdict>)> {
    let mut refs = Vec::new();
    let mut verdicts = Vec::new();
    if captures.is_empty() {
        return Ok((refs, verdicts));
    }
    std::fs::create_dir_all(cell_dir)?;
    for c in captures {
        let verdict = classify(&c.pcm, &ClassifyOptions::default());
        let wav = format!("{}.received.wav", c.agent);
        std::fs::write(cell_dir.join(&wav), wav_bytes(&c.pcm))?;

        let label = match (&verdict.classification, verdict.matched) {
            (Classification::Matched, Some(m)) => format!("{m:?}"),
            (other, _) => format!("{other:?}"),
        };
        let passed = verdict.classification == Classification::Matched
            && verdict.matched == Some(c.expected);
        verdicts.push(CheckVerdict {
            on: format!("{}.media", c.agent),
            field: "hears".to_string(),
            op: CheckOp::Eq,
            expected: Some(format!("{:?}", c.expected)),
            actual: verdict.matched.map(|m| format!("{m:?}")),
            passed,
            detail: format!(
                "classified {label} (distance {:.3}, margin {:.3}, rms {:.3}, {} samples)",
                verdict.distance,
                verdict.margin,
                verdict.rms,
                c.pcm.len()
            ),
        });
        refs.push(MediaRef { agent: c.agent.clone(), wav, classify: label, rms: verdict.rms });
    }
    Ok((refs, verdicts))
}
