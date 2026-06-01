//! Media layer — RTP/RTCP framing, G.711 codecs, SDP offer/answer negotiation,
//! and a paced media transport bound on [`sip_net::SignalingNetwork`].
//!
//! Rust port of `../sipjsserver/src/media/`. The engine is written once and is
//! transport-agnostic: it runs unchanged over the simulated fabric (deterministic
//! tests under a paused tokio clock) and over real UDP. The two
//! [`RtpFraming`](rtp::RtpFraming) implementations — the hand-rolled RFC 3550
//! [`HandRolled`](rtp::HandRolled) and the webrtc-rs [`WebRtcRs`](rtp::WebRtcRs)
//! witness — cross-check each other's wire format, mirroring the TS
//! `MediaEndpointTs` / `MediaEndpointRtpJs` pair.

pub mod codec;
pub mod rtp;
pub mod sdp;
pub mod transport;

pub use transport::{MediaEndpoint, MediaSession, MediaTransport};

use codec::G711Codec;

/// A buffer of 16-bit linear PCM at a known sample rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmBuffer {
    pub pcm: Vec<i16>,
    pub sample_rate: u32,
}

/// A play script: raw PCM or an ordered sequence of sub-scripts.
#[derive(Debug, Clone)]
pub enum PlayScript {
    Pcm(Vec<i16>),
    Sequence(Vec<PlayScript>),
}

/// Why a session is being committed as the active peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitReason {
    EarlyPem,
    Confirmed,
}

/// Direction of a media stream in [`MediaStreamStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDirection {
    Inbound,
    Outbound,
}

/// Per-stream stats snapshot. Mirrors the TS `MediaStreamStats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStreamStats {
    pub direction: StreamDirection,
    pub ssrc: u32,
    pub codec: G711Codec,
    pub payload_type: u8,
    pub packets: u64,
    pub bytes: u64,
    pub rtcp_packets_sent: u64,
    pub rtcp_packets_received: u64,
    pub remote: Option<NetAddr>,
}

/// A demuxed inbound source (per remote+SSRC). Mirrors the TS `SourceBucket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBucket {
    pub remote: NetAddr,
    pub ssrc: u32,
    pub payload_type: u8,
    pub packets: u64,
    pub bytes: u64,
    pub pcm: PcmBuffer,
}

/// Options for [`MediaEndpoint::open`].
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    pub queue_max: Option<usize>,
    /// RTCP report interval; defaults to the standard 5 s.
    pub rtcp_interval_ms: Option<u64>,
    /// Frame duration (ptime); defaults to 20 ms.
    pub ptime_ms: Option<u64>,
}

/// A session was configured with a codec we can't encode/decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("media negotiation error: {reason}")]
pub struct MediaNegotiationError {
    pub reason: String,
}

/// A network address (IP + UDP port) for media. Mirrors the TS `NetAddr`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetAddr {
    pub ip: String,
    pub port: u16,
}

impl NetAddr {
    pub fn new(ip: impl Into<String>, port: u16) -> Self {
        Self {
            ip: ip.into(),
            port,
        }
    }
}

/// A negotiable codec description. Mirrors the TS `CodecDesc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecDesc {
    pub name: G711Codec,
    pub payload_type: u8,
    /// All G.711 codecs are 8 kHz.
    pub clock_rate: u32,
}

/// PCMA (A-law), payload type 8.
pub const PCMA: CodecDesc = CodecDesc {
    name: G711Codec::Pcma,
    payload_type: 8,
    clock_rate: 8000,
};

/// PCMU (µ-law), payload type 0.
pub const PCMU: CodecDesc = CodecDesc {
    name: G711Codec::Pcmu,
    payload_type: 0,
    clock_rate: 8000,
};

use std::sync::Arc;

use sip_clock::Clock;
use sip_net::SignalingNetwork;

/// A [`MediaEndpoint`] using the hand-rolled RFC 3550 framing (the TS
/// `MediaEndpointTs`). Uses a test clock anchored at 0 for deterministic RTCP
/// timestamps under a paused runtime.
pub fn ts_endpoint(net: Arc<dyn SignalingNetwork>) -> MediaEndpoint {
    MediaEndpoint::with_clock(net, Arc::new(rtp::HandRolled), Clock::test_at(0))
}

/// A [`MediaEndpoint`] using the webrtc-rs framing witness (the TS
/// `MediaEndpointRtpJs`).
pub fn webrtc_endpoint(net: Arc<dyn SignalingNetwork>) -> MediaEndpoint {
    MediaEndpoint::with_clock(net, Arc::new(rtp::WebRtcRs), Clock::test_at(0))
}
