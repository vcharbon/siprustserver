//! RTP packet framing (RFC 3550) — the [`RtpFraming`] seam plus the hand-rolled
//! [`HandRolled`] implementation.
//!
//! Port of `../sipjsserver/src/media/rtp/packet.ts`. The seam lets the transport
//! engine swap wire codecs so an independent implementation
//! ([`WebRtcRs`](super::webrtc_framing::WebRtcRs)) can cross-check this one —
//! the rtp.js-witness pattern from the TS port. The encoder always emits the
//! 12-byte fixed header (CSRC count 0); the parser is full-form so it can read
//! any peer's packets (CSRC list, extension header, padding trim).

pub const RTP_HEADER_BYTES: usize = 12;

/// The fixed RTP header fields we model. Mirrors the TS `RtpHeader`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

/// A parsed RTP packet: header + payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpFramed {
    pub header: RtpHeader,
    pub payload: Vec<u8>,
}

/// The wire-codec seam. Two implementations cross-check each other.
pub trait RtpFraming: Send + Sync {
    /// `"ts"` for the hand-rolled codec, `"rtp.js"` for the webrtc-rs witness
    /// (labels kept from the TS port for trace continuity).
    fn name(&self) -> &'static str;
    fn encode_rtp(&self, header: &RtpHeader, payload: &[u8]) -> Vec<u8>;
    fn parse_rtp(&self, bytes: &[u8]) -> Option<RtpFramed>;
}

/// Hand-rolled RFC 3550 framing — the primary under-test codec (the TS
/// `tsFraming`).
#[derive(Debug, Clone, Copy, Default)]
pub struct HandRolled;

/// Free-function encode (RFC 3550, CSRC count 0). Exposed for direct use/tests.
pub fn encode_rtp(header: &RtpHeader, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; RTP_HEADER_BYTES + payload.len()];
    buf[0] = ((header.version & 0x03) << 6)
        | if header.padding { 0x20 } else { 0 }
        | if header.extension { 0x10 } else { 0 };
    // CSRC count always 0 for our sender.
    buf[1] = if header.marker { 0x80 } else { 0 } | (header.payload_type & 0x7f);
    buf[2..4].copy_from_slice(&header.sequence_number.to_be_bytes());
    buf[4..8].copy_from_slice(&header.timestamp.to_be_bytes());
    buf[8..12].copy_from_slice(&header.ssrc.to_be_bytes());
    buf[RTP_HEADER_BYTES..].copy_from_slice(payload);
    buf
}

/// Free-function parse — full form honouring CSRC list, extension words and
/// padding trim, so it reads any peer's packets. Returns `None` on a malformed
/// or non-v2 packet.
pub fn parse_rtp(bytes: &[u8]) -> Option<RtpFramed> {
    if bytes.len() < RTP_HEADER_BYTES {
        return None;
    }
    let b0 = bytes[0];
    let version = (b0 >> 6) & 0x03;
    if version != 2 {
        return None;
    }
    let padding = (b0 & 0x20) != 0;
    let extension = (b0 & 0x10) != 0;
    let csrc_count = (b0 & 0x0f) as usize;
    let b1 = bytes[1];
    let marker = (b1 & 0x80) != 0;
    let payload_type = b1 & 0x7f;
    let sequence_number = u16::from_be_bytes([bytes[2], bytes[3]]);
    let timestamp = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let ssrc = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);

    let mut offset = RTP_HEADER_BYTES + csrc_count * 4;
    if extension {
        if offset + 4 > bytes.len() {
            return None;
        }
        let ext_words = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        offset += 4 + ext_words * 4;
    }
    if offset > bytes.len() {
        return None;
    }

    let mut end = bytes.len();
    if padding && end > offset {
        let pad_bytes = bytes[end - 1] as usize;
        if pad_bytes > 0 && end >= offset + pad_bytes {
            end -= pad_bytes;
        }
    }

    Some(RtpFramed {
        header: RtpHeader {
            version,
            padding,
            extension,
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
        },
        payload: bytes[offset..end].to_vec(),
    })
}

impl RtpFraming for HandRolled {
    fn name(&self) -> &'static str {
        "ts"
    }
    fn encode_rtp(&self, header: &RtpHeader, payload: &[u8]) -> Vec<u8> {
        encode_rtp(header, payload)
    }
    fn parse_rtp(&self, bytes: &[u8]) -> Option<RtpFramed> {
        parse_rtp(bytes)
    }
}
