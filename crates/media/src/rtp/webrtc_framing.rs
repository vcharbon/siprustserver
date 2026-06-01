//! webrtc-rs RTP framing — the independent conformance witness.
//!
//! Wraps the `rtp` crate's [`Packet`](rtp::packet::Packet) /
//! [`Header`](rtp::header::Header) so it can cross-check the hand-rolled
//! [`HandRolled`](super::packet::HandRolled) codec on the wire. Direct analog of
//! the TS `rtpJsFraming` (which wraps Versatica's `rtp.js`).

use bytes::Bytes;
use rtp::header::Header;
use rtp::packet::Packet;
use webrtc_util::marshal::{Marshal, Unmarshal};

use super::packet::{RtpFramed, RtpFraming, RtpHeader};

/// webrtc-rs framing witness (the rtp.js role).
#[derive(Debug, Clone, Copy, Default)]
pub struct WebRtcRs;

impl RtpFraming for WebRtcRs {
    fn name(&self) -> &'static str {
        "rtp.js"
    }

    fn encode_rtp(&self, header: &RtpHeader, payload: &[u8]) -> Vec<u8> {
        let pkt = Packet {
            header: Header {
                version: 2,
                padding: false,
                extension: false,
                marker: header.marker,
                payload_type: header.payload_type,
                sequence_number: header.sequence_number,
                timestamp: header.timestamp,
                ssrc: header.ssrc,
                csrc: vec![],
                extension_profile: 0,
                extensions: vec![],
                extensions_padding: 0,
            },
            payload: Bytes::copy_from_slice(payload),
        };
        pkt.marshal().map(|b| b.to_vec()).unwrap_or_default()
    }

    fn parse_rtp(&self, bytes: &[u8]) -> Option<RtpFramed> {
        let mut buf = Bytes::copy_from_slice(bytes);
        let pkt = Packet::unmarshal(&mut buf).ok()?;
        if pkt.header.version != 2 {
            return None;
        }
        Some(RtpFramed {
            header: RtpHeader {
                version: pkt.header.version,
                padding: pkt.header.padding,
                extension: pkt.header.extension,
                marker: pkt.header.marker,
                payload_type: pkt.header.payload_type,
                sequence_number: pkt.header.sequence_number,
                timestamp: pkt.header.timestamp,
                ssrc: pkt.header.ssrc,
            },
            payload: pkt.payload.to_vec(),
        })
    }
}
