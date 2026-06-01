//! G.711 codec — PCMA (A-law, PT 8) and PCMU (µ-law, PT 0).
//!
//! Pure conversions between 16-bit linear PCM and the 8-bit companded G.711
//! representations. Used by the media RTP stack to frame audio and by the audio
//! comparator's transcoding-robustness test. This is the canonical ITU-T G.711
//! reference algorithm (the Sun/CCITT implementation): A-law works on the top 13
//! bits, µ-law on the top 14, with the standard segment search.
//!
//! Port of `../sipjsserver/src/media/codec/g711.ts`.

const SIGN_BIT: i32 = 0x80;
const QUANT_MASK: i32 = 0x0f;
const SEG_SHIFT: i32 = 4;
const SEG_MASK: i32 = 0x70;
const BIAS: i32 = 0x84;
const MU_CLIP: i32 = 8159;

const SEG_AEND: [i32; 8] = [0x1f, 0x3f, 0x7f, 0xff, 0x1ff, 0x3ff, 0x7ff, 0xfff];
const SEG_UEND: [i32; 8] = [0x3f, 0x7f, 0xff, 0x1ff, 0x3ff, 0x7ff, 0xfff, 0x1fff];

fn segment_search(val: i32, table: &[i32; 8]) -> i32 {
    for (i, &bound) in table.iter().enumerate() {
        if val <= bound {
            return i as i32;
        }
    }
    table.len() as i32
}

/// Encode one PCM16 sample to an A-law byte.
pub fn alaw_encode_sample(pcm: i16) -> u8 {
    let mask: i32;
    let mut value = (pcm as i32) >> 3; // scale to 13-bit
    if value >= 0 {
        mask = 0xd5; // sign bit = 1 for positive
    } else {
        mask = 0x55;
        value = -value - 1;
    }
    let seg = segment_search(value, &SEG_AEND);
    if seg >= 8 {
        return ((0x7f ^ mask) & 0xff) as u8;
    }
    let mut aval = seg << SEG_SHIFT;
    aval |= if seg < 2 {
        (value >> 1) & QUANT_MASK
    } else {
        (value >> seg) & QUANT_MASK
    };
    ((aval ^ mask) & 0xff) as u8
}

/// Decode one A-law byte to a PCM16 sample.
pub fn alaw_decode_sample(alaw: u8) -> i16 {
    let a = ((alaw as i32) ^ 0x55) & 0xff;
    let mut t = (a & QUANT_MASK) << 4;
    let seg = (a & SEG_MASK) >> SEG_SHIFT;
    if seg == 0 {
        t += 8;
    } else if seg == 1 {
        t += 0x108;
    } else {
        t += 0x108;
        t <<= seg - 1;
    }
    if (a & SIGN_BIT) != 0 {
        t as i16
    } else {
        (-t) as i16
    }
}

/// Encode one PCM16 sample to a µ-law byte.
pub fn mulaw_encode_sample(pcm: i16) -> u8 {
    let mut value = (pcm as i32) >> 2; // scale to 14-bit
    let mask: i32;
    if value < 0 {
        value = -value;
        mask = 0x7f;
    } else {
        mask = 0xff;
    }
    if value > MU_CLIP {
        value = MU_CLIP;
    }
    value += BIAS >> 2;
    let seg = segment_search(value, &SEG_UEND);
    if seg >= 8 {
        return ((0x7f ^ mask) & 0xff) as u8;
    }
    let uval = (seg << 4) | ((value >> (seg + 1)) & 0x0f);
    ((uval ^ mask) & 0xff) as u8
}

/// Decode one µ-law byte to a PCM16 sample.
pub fn mulaw_decode_sample(mulaw: u8) -> i16 {
    let u = (!(mulaw as i32)) & 0xff;
    let mut t = ((u & QUANT_MASK) << 3) + BIAS;
    t <<= (u & SEG_MASK) >> SEG_SHIFT;
    if (u & SIGN_BIT) != 0 {
        (BIAS - t) as i16
    } else {
        (t - BIAS) as i16
    }
}

pub fn alaw_encode(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().map(|&s| alaw_encode_sample(s)).collect()
}

pub fn alaw_decode(alaw: &[u8]) -> Vec<i16> {
    alaw.iter().map(|&b| alaw_decode_sample(b)).collect()
}

pub fn mulaw_encode(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().map(|&s| mulaw_encode_sample(s)).collect()
}

pub fn mulaw_decode(mulaw: &[u8]) -> Vec<i16> {
    mulaw.iter().map(|&b| mulaw_decode_sample(b)).collect()
}

/// G.711 codec selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G711Codec {
    Pcma,
    Pcmu,
}

impl G711Codec {
    /// The SDP `a=rtpmap` encoding name.
    pub fn as_str(self) -> &'static str {
        match self {
            G711Codec::Pcma => "PCMA",
            G711Codec::Pcmu => "PCMU",
        }
    }

    /// Parse an SDP encoding name (case-sensitive, as written on the wire).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "PCMA" => Some(G711Codec::Pcma),
            "PCMU" => Some(G711Codec::Pcmu),
            _ => None,
        }
    }
}

/// Round-trip PCM through a G.711 codec — models what a transcoding leg does.
pub fn g711_round_trip(pcm: &[i16], codec: G711Codec) -> Vec<i16> {
    match codec {
        G711Codec::Pcma => alaw_decode(&alaw_encode(pcm)),
        G711Codec::Pcmu => mulaw_decode(&mulaw_encode(pcm)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A sweep of representative PCM16 levels.
    fn sweep() -> Vec<i16> {
        let mut v = vec![0i16, 1, -1, 100, -100, 1000, -1000];
        v.extend([i16::MAX, i16::MIN, 16384, -16384, 8159, -8159, 4096, -4096]);
        for k in -32000..=32000i32 {
            if k % 257 == 0 {
                v.push(k as i16);
            }
        }
        v
    }

    // The exact codec invariant for a companding codec: every code byte
    // decodes to a representative PCM level that re-encodes to the same byte.
    // (PCM round-trip is necessarily lossy — encode is many-to-one — so the
    // meaningful invariant is byte→pcm→byte stability, which is exact.)
    #[test]
    fn alaw_byte_round_trip_is_exact() {
        for b in 0u16..256 {
            let b = b as u8;
            assert_eq!(alaw_encode_sample(alaw_decode_sample(b)), b, "alaw byte {b:#x}");
        }
    }

    #[test]
    fn mulaw_byte_round_trip_is_exact_modulo_negative_zero() {
        for b in 0u16..256 {
            let b = b as u8;
            let pcm = mulaw_decode_sample(b);
            let re = mulaw_encode_sample(pcm);
            // µ-law has a redundant negative-zero code (0x7f): it decodes to 0,
            // which re-encodes to the canonical +0 idle byte 0xff. Every other
            // code round-trips exactly.
            if pcm == 0 {
                assert_eq!(re, 0xff, "mulaw zero code {b:#x}");
            } else {
                assert_eq!(re, b, "mulaw byte {b:#x}");
            }
        }
    }

    // Encode is idempotent through decode: quantising a sample, dequantising it,
    // then re-quantising lands on the same code (the decoded value sits inside
    // its own quantisation cell).
    #[test]
    fn encode_is_idempotent_through_decode() {
        for &s in &sweep() {
            let a = alaw_encode_sample(s);
            assert_eq!(alaw_encode_sample(alaw_decode_sample(a)), a, "alaw {s}");
            let u = mulaw_encode_sample(s);
            assert_eq!(mulaw_encode_sample(mulaw_decode_sample(u)), u, "mulaw {s}");
        }
    }

    #[test]
    fn bulk_helpers_match_per_sample() {
        let pcm: Vec<i16> = sweep();
        assert_eq!(alaw_encode(&pcm), pcm.iter().map(|&s| alaw_encode_sample(s)).collect::<Vec<_>>());
        assert_eq!(
            g711_round_trip(&pcm, G711Codec::Pcma),
            alaw_decode(&alaw_encode(&pcm))
        );
        assert_eq!(
            g711_round_trip(&pcm, G711Codec::Pcmu),
            mulaw_decode(&mulaw_encode(&pcm))
        );
    }

    #[test]
    fn known_vectors() {
        // Silence encodes to the canonical idle bytes (A-law 0xd5, µ-law 0xff).
        assert_eq!(alaw_encode_sample(0), 0xd5);
        assert_eq!(mulaw_encode_sample(0), 0xff);
    }
}
