//! Media transport engine — one local RTP port over the shared
//! [`SignalingNetwork`](sip_net::SignalingNetwork), paced on `tokio::time`.
//!
//! Port of `../sipjsserver/src/media/transport.ts`. Parameterized by an
//! [`RtpFraming`](crate::rtp::RtpFraming) so the hand-rolled and webrtc-rs
//! implementations share this whole engine and differ only in the wire codec
//! (each an independent witness of the other).
//!
//! It demuxes inbound RTP into per-(remote, SSRC) source buckets and records
//! each continuously (regardless of play state). It carries one or more
//! per-dialog [`MediaSession`]s; at most one is the committed **active peer** —
//! the only session we send from, and the one a plain `recorded()` resolves
//! against. Committing one session abandons the rest (their senders stop);
//! inbound recording continues for all sources so forked early media is still
//! observable for the report. RTP and RTCP are muxed on the one port (RFC 5761);
//! RTCP is counts-only.
//!
//! **Timing discipline (per CLAUDE.md):** behavioural pacing uses
//! `tokio::time::sleep` directly so it rides the paused test clock; the
//! [`Clock`](sip_clock::Clock) is consulted only for RTCP NTP timestamps. The
//! shared state lock is never held across an `.await`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_net::{BindError, BindUdpOpts, SignalingNetwork, UdpEndpoint};
use tokio::task::JoinHandle;

use crate::codec::{alaw_decode, alaw_encode, mulaw_decode, mulaw_encode, G711Codec};
use crate::rtp::{
    encode_receiver_report, encode_sender_report, is_rtcp, RtpFraming, RtpHeader, SenderReportFields,
};
use crate::sdp::NegotiatedMedia;
use crate::{
    CodecDesc, CommitReason, MediaNegotiationError, MediaStreamStats, NetAddr, OpenOptions,
    PcmBuffer, PlayScript, SourceBucket, StreamDirection, PCMA, PCMU,
};

const SAMPLE_RATE: u32 = 8000;
const DEFAULT_PTIME_MS: u64 = 20;
const DEFAULT_RTCP_INTERVAL_MS: u64 = 5000;
const DEFAULT_QUEUE_MAX: usize = 2048;
/// Base of the endpoint's auto-allocated RTP port range (even ports, +2 step).
const AUTO_PORT_BASE: u16 = 40000;

/// FNV-1a over `ip:port` — the local SSRC. Matches the TS `deriveSsrc`.
fn derive_ssrc(ip: &str, port: u16) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in format!("{ip}:{port}").bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn encode_pcm(pcm: &[i16], codec: &CodecDesc) -> Vec<u8> {
    match codec.name {
        G711Codec::Pcma => alaw_encode(pcm),
        G711Codec::Pcmu => mulaw_encode(pcm),
    }
}

fn decode_payload(payload: &[u8], payload_type: u8) -> Vec<i16> {
    if payload_type == PCMU.payload_type {
        mulaw_decode(payload)
    } else {
        alaw_decode(payload)
    }
}

/// Flatten a (possibly nested) [`PlayScript`] into one continuous PCM stream.
fn flatten_script(script: &PlayScript, out: &mut Vec<i16>) {
    match script {
        PlayScript::Pcm(pcm) => out.extend_from_slice(pcm),
        PlayScript::Sequence(parts) => {
            for p in parts {
                flatten_script(p, out);
            }
        }
    }
}

struct InternalBucket {
    ssrc: u32,
    remote: NetAddr,
    payload_type: u8,
    packets: u64,
    bytes: u64,
    pcm: Vec<i16>,
}

struct SessionState {
    negotiated: Option<NegotiatedMedia>,
    committed: bool,
    abandoned: bool,
}

/// Shared per-transport state. Guarded by a single mutex held only for short,
/// await-free critical sections.
#[derive(Default)]
struct Inner {
    seq: u16,
    rtp_timestamp: u32,
    out_packets: u64,
    out_bytes: u64,
    out_codec: Option<CodecDesc>,
    last_remote: Option<NetAddr>,
    rtcp_sent: u64,
    rtcp_received: u64,
    sources: HashMap<String, InternalBucket>,
    sessions: HashMap<String, SessionState>,
    active_session_id: Option<String>,
}

fn parse_dst(remote: &NetAddr) -> Option<SocketAddr> {
    format!("{}:{}", remote.ip, remote.port).parse().ok()
}

fn merge_pcm(buckets: &[&InternalBucket]) -> PcmBuffer {
    let mut merged = Vec::new();
    for b in buckets {
        merged.extend_from_slice(&b.pcm);
    }
    PcmBuffer {
        pcm: merged,
        sample_rate: SAMPLE_RATE,
    }
}

/// A media endpoint factory bound to a [`SignalingNetwork`] and an
/// [`RtpFraming`] wire codec. Each [`open`](MediaEndpoint::open) binds one RTP
/// port. Mirrors the TS `mediaEndpointLayer`.
#[derive(Clone)]
pub struct MediaEndpoint {
    net: Arc<dyn SignalingNetwork>,
    framing: Arc<dyn RtpFraming>,
    next_auto_port: Arc<AtomicU16>,
    clock: Clock,
}

impl MediaEndpoint {
    /// Build an endpoint over the given network and framing. Uses a wall-clock
    /// for RTCP timestamps; pass [`with_clock`](Self::with_clock) in tests.
    pub fn new(net: Arc<dyn SignalingNetwork>, framing: Arc<dyn RtpFraming>) -> Self {
        Self::with_clock(net, framing, Clock::system())
    }

    pub fn with_clock(
        net: Arc<dyn SignalingNetwork>,
        framing: Arc<dyn RtpFraming>,
        clock: Clock,
    ) -> Self {
        Self {
            net,
            framing,
            next_auto_port: Arc::new(AtomicU16::new(AUTO_PORT_BASE)),
            clock,
        }
    }

    /// Bind one RTP port and start its inbound recorder + RTCP reporter.
    pub async fn open(
        &self,
        local_ip: &str,
        local_port: Option<u16>,
        opts: OpenOptions,
    ) -> Result<MediaTransport, BindError> {
        let ptime_ms = opts.ptime_ms.unwrap_or(DEFAULT_PTIME_MS);
        let rtcp_interval_ms = opts.rtcp_interval_ms.unwrap_or(DEFAULT_RTCP_INTERVAL_MS);
        let samples_per_frame = ((ptime_ms as f64 / 1000.0) * SAMPLE_RATE as f64).round() as usize;
        let port = local_port.unwrap_or_else(|| self.next_auto_port.fetch_add(2, Ordering::Relaxed));

        let addr: SocketAddr = format!("{local_ip}:{port}")
            .parse()
            .map_err(|e| BindError {
                reason: sip_net::BindErrorReason::OsError,
                addr: format!("0.0.0.0:{port}").parse().unwrap(),
                message: format!("bad media addr {local_ip}:{port}: {e}"),
            })?;
        let queue_max = opts.queue_max.unwrap_or(DEFAULT_QUEUE_MAX);
        let endpoint: Arc<dyn UdpEndpoint> =
            Arc::from(self.net.bind_udp(BindUdpOpts::new(addr, queue_max)).await?);

        // Read the actually-bound address back — with port 0 the OS assigns a
        // real port, and that (not the requested 0) is what we advertise and
        // key our SSRC on.
        let bound = endpoint.local_addr();
        let local_addr = NetAddr::new(bound.ip().to_string(), bound.port());
        let ssrc = derive_ssrc(&local_addr.ip, local_addr.port);
        let inner = Arc::new(Mutex::new(Inner::default()));
        let framing = self.framing.clone();

        // Continuous inbound recorder (all sources).
        let recorder = {
            let inner = inner.clone();
            let framing = framing.clone();
            let ep = endpoint.clone();
            tokio::spawn(async move {
                while let Some(pkt) = ep.recv().await {
                    handle_packet(&inner, framing.as_ref(), &pkt.raw, pkt.src);
                }
            })
        };

        // RTCP reporter — counts only. SR once we've sent media, else RR.
        let reporter = {
            let inner = inner.clone();
            let ep = endpoint.clone();
            let clock = self.clock.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(rtcp_interval_ms)).await;
                    let (report, dst) = {
                        let mut g = inner.lock().unwrap();
                        let Some(remote) = g.last_remote.clone() else {
                            continue;
                        };
                        let report = if g.out_packets > 0 {
                            encode_sender_report(&SenderReportFields {
                                ssrc,
                                ntp_ms: clock.now_ms(),
                                rtp_timestamp: g.rtp_timestamp,
                                packet_count: g.out_packets as u32,
                                octet_count: g.out_bytes as u32,
                            })
                        } else {
                            encode_receiver_report(ssrc)
                        };
                        g.rtcp_sent += 1;
                        (report, parse_dst(&remote))
                    };
                    if let Some(dst) = dst {
                        let _ = ep.send_to(&report, dst).await;
                    }
                }
            })
        };

        Ok(MediaTransport {
            local_addr,
            ssrc,
            ptime_ms,
            samples_per_frame,
            endpoint,
            framing,
            inner,
            tasks: Arc::new(vec![recorder, reporter]),
        })
    }
}

/// Demux one inbound datagram into its source bucket (or count it as RTCP).
fn handle_packet(inner: &Mutex<Inner>, framing: &dyn RtpFraming, raw: &[u8], src: SocketAddr) {
    if is_rtcp(raw) {
        inner.lock().unwrap().rtcp_received += 1;
        return;
    }
    let Some(parsed) = framing.parse_rtp(raw) else {
        return;
    };
    let rip = src.ip().to_string();
    let rport = src.port();
    let key = format!("{rip}:{rport}:{}", parsed.header.ssrc);
    let mut g = inner.lock().unwrap();
    let bucket = g.sources.entry(key).or_insert_with(|| InternalBucket {
        ssrc: parsed.header.ssrc,
        remote: NetAddr::new(rip, rport),
        payload_type: parsed.header.payload_type,
        packets: 0,
        bytes: 0,
        pcm: Vec::new(),
    });
    bucket.packets += 1;
    bucket.bytes += raw.len() as u64;
    bucket.payload_type = parsed.header.payload_type;
    let decoded = decode_payload(&parsed.payload, parsed.header.payload_type);
    bucket.pcm.extend_from_slice(&decoded);
}

/// One bound RTP port. Cheap to clone — sessions and stats all share the same
/// inner state. Mirrors the TS `MediaTransport`.
#[derive(Clone)]
pub struct MediaTransport {
    local_addr: NetAddr,
    ssrc: u32,
    ptime_ms: u64,
    samples_per_frame: usize,
    endpoint: Arc<dyn UdpEndpoint>,
    framing: Arc<dyn RtpFraming>,
    inner: Arc<Mutex<Inner>>,
    tasks: Arc<Vec<JoinHandle<()>>>,
}

impl MediaTransport {
    pub fn local_addr(&self) -> &NetAddr {
        &self.local_addr
    }

    pub fn supported_codecs(&self) -> [CodecDesc; 2] {
        [PCMA, PCMU]
    }

    /// Get (or lazily create) the per-dialog session.
    pub fn session(&self, dialog_id: &str) -> MediaSession {
        let mut g = self.inner.lock().unwrap();
        g.sessions.entry(dialog_id.to_string()).or_insert(SessionState {
            negotiated: None,
            committed: false,
            abandoned: false,
        });
        drop(g);
        MediaSession {
            dialog_id: dialog_id.to_string(),
            transport: self.clone(),
        }
    }

    /// Snapshot of every demuxed inbound source.
    pub fn sources(&self) -> Vec<SourceBucket> {
        let g = self.inner.lock().unwrap();
        g.sources
            .values()
            .map(|b| SourceBucket {
                remote: b.remote.clone(),
                ssrc: b.ssrc,
                payload_type: b.payload_type,
                packets: b.packets,
                bytes: b.bytes,
                pcm: merge_pcm(&[b]),
            })
            .collect()
    }

    /// The currently committed active-peer session, if any.
    pub fn active_peer(&self) -> Option<MediaSession> {
        let g = self.inner.lock().unwrap();
        g.active_session_id.clone().map(|id| MediaSession {
            dialog_id: id,
            transport: self.clone(),
        })
    }

    /// Per-stream stats (one outbound if we've sent, plus one per inbound source).
    pub fn stats(&self) -> Vec<MediaStreamStats> {
        let g = self.inner.lock().unwrap();
        let mut out = Vec::new();
        if let (Some(codec), Some(remote)) = (g.out_codec, g.last_remote.clone()) {
            out.push(MediaStreamStats {
                direction: StreamDirection::Outbound,
                ssrc: self.ssrc,
                codec: codec.name,
                payload_type: codec.payload_type,
                packets: g.out_packets,
                bytes: g.out_bytes,
                rtcp_packets_sent: g.rtcp_sent,
                rtcp_packets_received: g.rtcp_received,
                remote: Some(remote),
            });
        }
        for b in g.sources.values() {
            let codec = if b.payload_type == PCMU.payload_type {
                G711Codec::Pcmu
            } else {
                G711Codec::Pcma
            };
            out.push(MediaStreamStats {
                direction: StreamDirection::Inbound,
                ssrc: b.ssrc,
                codec,
                payload_type: b.payload_type,
                packets: b.packets,
                bytes: b.bytes,
                rtcp_packets_sent: 0,
                rtcp_packets_received: g.rtcp_received,
                remote: Some(b.remote.clone()),
            });
        }
        out
    }

    /// Inject raw bytes to a remote — escape hatch for negative wire tests.
    pub async fn send_raw(&self, bytes: &[u8], remote: &NetAddr) {
        if let Some(dst) = parse_dst(remote) {
            let _ = self.endpoint.send_to(bytes, dst).await;
        }
    }
}

impl Drop for MediaTransport {
    fn drop(&mut self) {
        // Last owner closes shop: abort the background tasks (the scope-close
        // analogue of the TS `forkIn`).
        if Arc::strong_count(&self.tasks) == 1 {
            for t in self.tasks.iter() {
                t.abort();
            }
        }
    }
}

/// A per-dialog media session. Mirrors the TS `MediaSession`.
#[derive(Clone)]
pub struct MediaSession {
    dialog_id: String,
    transport: MediaTransport,
}

impl MediaSession {
    pub fn dialog_id(&self) -> &str {
        &self.dialog_id
    }

    /// Set codec, remote addr and direction — from the negotiation engine's
    /// [`NegotiatedMedia`].
    pub fn configure(&self, remote: NegotiatedMedia) -> Result<(), MediaNegotiationError> {
        // PCMA/PCMU are the only codecs we encode; the engine only ever yields
        // those, but mirror the TS guard for defence in depth.
        let mut g = self.transport.inner.lock().unwrap();
        if let Some(st) = g.sessions.get_mut(&self.dialog_id) {
            st.negotiated = Some(remote);
        }
        Ok(())
    }

    /// Become the committed active peer; abandons sibling sessions.
    pub fn commit(&self, _reason: CommitReason) {
        let mut g = self.transport.inner.lock().unwrap();
        let remote = {
            let st = match g.sessions.get_mut(&self.dialog_id) {
                Some(st) => st,
                None => return,
            };
            st.committed = true;
            st.abandoned = false;
            st.negotiated.as_ref().map(|n| n.remote.clone())
        };
        g.active_session_id = Some(self.dialog_id.clone());
        for (id, other) in g.sessions.iter_mut() {
            if id != &self.dialog_id {
                other.committed = false;
                other.abandoned = true;
            }
        }
        if let Some(r) = remote {
            g.last_remote = Some(r);
        }
    }

    /// Non-blocking; spawns a paced sender that only emits once this session is
    /// the committed, send-enabled active peer.
    pub fn play(&self, script: PlayScript) {
        let neg = {
            let mut g = self.transport.inner.lock().unwrap();
            let st = match g.sessions.get(&self.dialog_id) {
                Some(st) => st,
                None => return,
            };
            let active = g.active_session_id.as_deref() == Some(self.dialog_id.as_str());
            match &st.negotiated {
                Some(n) if st.committed && active && n.send => {
                    let neg = n.clone();
                    g.out_codec = Some(neg.codec);
                    g.last_remote = Some(neg.remote.clone());
                    neg
                }
                // Not the active, send-enabled peer → stay silent.
                _ => return,
            }
        };

        let mut pcm = Vec::new();
        flatten_script(&script, &mut pcm);

        let dialog_id = self.dialog_id.clone();
        let inner = self.transport.inner.clone();
        let framing = self.transport.framing.clone();
        let endpoint = self.transport.endpoint.clone();
        let ssrc = self.transport.ssrc;
        let ptime_ms = self.transport.ptime_ms;
        let samples_per_frame = self.transport.samples_per_frame.max(1);
        let codec = neg.codec;
        let Some(dst) = parse_dst(&neg.remote) else {
            return;
        };

        tokio::spawn(async move {
            let mut first = true;
            let mut off = 0usize;
            while off < pcm.len() {
                // Build the frame under the lock (never held across the await).
                let packet = {
                    let mut g = inner.lock().unwrap();
                    // Stop if this session lost the active-peer role mid-play.
                    let still_active = g.active_session_id.as_deref() == Some(dialog_id.as_str());
                    let abandoned = g
                        .sessions
                        .get(&dialog_id)
                        .map(|s| s.abandoned)
                        .unwrap_or(true);
                    if !still_active || abandoned {
                        return;
                    }
                    let end = (off + samples_per_frame).min(pcm.len());
                    let chunk = &pcm[off..end];
                    let payload = encode_pcm(chunk, &codec);
                    g.rtp_timestamp = g.rtp_timestamp.wrapping_add(chunk.len() as u32);
                    g.seq = g.seq.wrapping_add(1);
                    let header = RtpHeader {
                        version: 2,
                        padding: false,
                        extension: false,
                        marker: first,
                        payload_type: codec.payload_type,
                        sequence_number: g.seq,
                        timestamp: g.rtp_timestamp,
                        ssrc,
                    };
                    let packet = framing.encode_rtp(&header, &payload);
                    g.out_packets += 1;
                    g.out_bytes += packet.len() as u64;
                    packet
                };
                first = false;
                off += samples_per_frame;
                let _ = endpoint.send_to(&packet, dst).await;
                tokio::time::sleep(Duration::from_millis(ptime_ms)).await;
            }
        });
    }

    /// Inbound PCM attributed to this peer (matched by negotiated remote).
    pub fn recorded(&self) -> PcmBuffer {
        let g = self.transport.inner.lock().unwrap();
        let remote = match g.sessions.get(&self.dialog_id).and_then(|s| s.negotiated.as_ref()) {
            Some(n) => n.remote.clone(),
            None => {
                return PcmBuffer {
                    pcm: Vec::new(),
                    sample_rate: SAMPLE_RATE,
                }
            }
        };
        let matching: Vec<&InternalBucket> =
            g.sources.values().filter(|b| b.remote == remote).collect();
        merge_pcm(&matching)
    }

    pub fn is_active(&self) -> bool {
        let g = self.transport.inner.lock().unwrap();
        g.active_session_id.as_deref() == Some(self.dialog_id.as_str())
            && g.sessions
                .get(&self.dialog_id)
                .map(|s| !s.abandoned)
                .unwrap_or(false)
    }
}
