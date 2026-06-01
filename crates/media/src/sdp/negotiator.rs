//! Per-dialog RFC 3264 / 3262 offer-answer engine — the endpoint's equivalent
//! of WebRTC setLocalDescription / setRemoteDescription, and an *independent*
//! conformance witness of whatever SDP the B2BUA relays or synthesizes.
//!
//! Port of `../sipjsserver/src/media/sdp/negotiator.ts`. Strict refusals (typed
//! [`SdpNegotiationError`], each a named RFC MUST) catch a B2BUA that mangles the
//! relayed c=/m=, intersects codecs wrong, or drops an m-line: the receiving
//! engine rejects or mis-points, so a media `hears(...)` assertion fails loudly.

use crate::codec::G711Codec;
use crate::{CodecDesc, NetAddr};

use super::{
    build_sdp, media_connection_addr, MediaDirection, Sdp, SdpMedia, SdpOrigin, SdpRtpMap,
};

/// The negotiation state machine. Mirrors the TS `NegotiationState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationState {
    Idle,
    OfferSent,
    Early,
    Committed,
    Held,
}

/// The committed outcome of negotiation. Mirrors the TS `NegotiatedMedia`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedMedia {
    pub remote: NetAddr,
    pub codec: CodecDesc,
    pub direction: MediaDirection,
    pub send: bool,
    pub receive: bool,
}

/// Which RFC MUST a refusal violated. Each variant carries its clause and a
/// human message via [`SdpNegotiationError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpRule {
    /// RFC 3264 §6: answer PT MUST come from the offer.
    AnswerCodecNotInOffer,
    /// RFC 3264 §6: no common codec.
    EmptyCodecIntersection,
    /// RFC 3264 §6: answer MUST have the same m-line count as the offer.
    MLineCountMismatch,
    /// RFC 3264 §5: no offer outstanding.
    AnswerWithoutOffer,
    /// RFC 3264 §4 / RFC 3261 §14.2: an offer arrived while ours is outstanding (→ 491).
    GlareSecondOffer,
    /// RFC 3264 §5.1: no usable media description.
    NoMedia,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{rule:?} ({rfc}): {message}")]
pub struct SdpNegotiationError {
    pub rule: SdpRule,
    pub rfc: &'static str,
    pub message: String,
}

fn err(rule: SdpRule, rfc: &'static str, message: impl Into<String>) -> SdpNegotiationError {
    SdpNegotiationError {
        rule,
        rfc,
        message: message.into(),
    }
}

/// Static payload-type → encoding name map for codecs that omit `a=rtpmap`.
fn static_pt_name(pt: u8) -> Option<&'static str> {
    match pt {
        0 => Some("PCMU"),
        8 => Some("PCMA"),
        _ => None,
    }
}

fn resolve_codec_name(media: &SdpMedia, pt: u8) -> Option<String> {
    media
        .rtpmap
        .iter()
        .find(|r| r.payload_type == pt)
        .map(|r| r.encoding_name.clone())
        .or_else(|| static_pt_name(pt).map(str::to_string))
}

fn is_reachable(addr: &str, port: u16) -> bool {
    port != 0 && addr != "0.0.0.0" && addr != "::"
}

/// FNV-1a over `ip:port`, matching the TS `deriveSessionId`.
fn derive_session_id(addr: &NetAddr) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    let s = format!("{}:{}", addr.ip, addr.port);
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The per-dialog offer/answer engine. Mirrors the TS `OfferAnswerEngine`.
pub struct OfferAnswerEngine {
    local_addr: NetAddr,
    codecs: Vec<CodecDesc>,
    username: String,
    state: NegotiationState,
    pending_offer: Option<Sdp>,
    negotiated: Option<NegotiatedMedia>,
    session_version: u64,
}

impl OfferAnswerEngine {
    /// Build an engine bound to our local RTP address and codec preference list.
    pub fn new(local_addr: NetAddr, codecs: Vec<CodecDesc>) -> Self {
        Self::with_username(local_addr, codecs, "media")
    }

    pub fn with_username(local_addr: NetAddr, codecs: Vec<CodecDesc>, username: &str) -> Self {
        Self {
            local_addr,
            codecs,
            username: username.to_string(),
            state: NegotiationState::Idle,
            pending_offer: None,
            negotiated: None,
            session_version: 1,
        }
    }

    fn codec_by_name(&self, name: &str) -> Option<CodecDesc> {
        let g = G711Codec::from_name(name)?;
        self.codecs.iter().copied().find(|c| c.name == g)
    }

    fn build_local_sdp(&self, codecs: &[CodecDesc], direction: MediaDirection) -> Sdp {
        Sdp {
            origin: SdpOrigin {
                username: self.username.clone(),
                session_id: derive_session_id(&self.local_addr).to_string(),
                version: self.session_version.to_string(),
                address: self.local_addr.ip.clone(),
            },
            connection_addr: Some(self.local_addr.ip.clone()),
            media: vec![SdpMedia {
                kind: "audio".into(),
                port: self.local_addr.port,
                protocol: "RTP/AVP".into(),
                formats: codecs.iter().map(|c| c.payload_type).collect(),
                rtpmap: codecs
                    .iter()
                    .map(|c| SdpRtpMap {
                        payload_type: c.payload_type,
                        encoding_name: c.name.as_str().to_string(),
                        clock_rate: c.clock_rate,
                    })
                    .collect(),
                direction,
                connection_addr: None,
            }],
        }
    }

    /// Build an offer from the bound session (real addr/port + our codec list).
    pub fn local_offer(&mut self) -> Sdp {
        self.session_version += 1;
        let offer = self.build_local_sdp(&self.codecs.clone(), MediaDirection::SendRecv);
        self.pending_offer = Some(offer.clone());
        self.state = NegotiationState::OfferSent;
        offer
    }

    /// UAS: build a strict final answer to a received offer; configures our send side.
    pub fn answer_to(&mut self, remote_offer: &Sdp) -> Result<Sdp, SdpNegotiationError> {
        if self.state == NegotiationState::OfferSent {
            return Err(err(
                SdpRule::GlareSecondOffer,
                "RFC 3264 §4 / RFC 3261 §14.2",
                "received an offer while our own offer is outstanding (glare → 491)",
            ));
        }
        let m = remote_offer.audio_media().ok_or_else(|| {
            err(
                SdpRule::NoMedia,
                "RFC 3264 §5.1",
                "offer has no audio media description",
            )
        })?;

        // Pick the first offered codec we support (honours offerer preference).
        let mut chosen: Option<CodecDesc> = None;
        for &pt in &m.formats {
            if let Some(name) = resolve_codec_name(m, pt) {
                if let Some(c) = self.codec_by_name(&name) {
                    chosen = Some(c);
                    break;
                }
            }
        }
        let chosen = chosen.ok_or_else(|| {
            err(
                SdpRule::EmptyCodecIntersection,
                "RFC 3264 §6",
                "no codec in common between offer and our supported set",
            )
        })?;

        let remote_addr = media_connection_addr(remote_offer, m);
        let answer_dir = m.direction.reverse();
        let reachable = is_reachable(&remote_addr, m.port);
        let send = matches!(answer_dir, MediaDirection::SendRecv | MediaDirection::SendOnly)
            && reachable;
        let receive = matches!(answer_dir, MediaDirection::SendRecv | MediaDirection::RecvOnly);

        self.negotiated = Some(NegotiatedMedia {
            remote: NetAddr::new(remote_addr, m.port),
            codec: chosen,
            direction: answer_dir,
            send,
            receive,
        });
        self.state = if send {
            NegotiationState::Committed
        } else {
            NegotiationState::Held
        };
        self.session_version += 1;
        Ok(self.build_local_sdp(&[chosen], answer_dir))
    }

    /// UAC: apply a received answer (provisional or final); re-points the session.
    pub fn apply_remote(
        &mut self,
        sdp: &Sdp,
        reliable: bool,
    ) -> Result<NegotiatedMedia, SdpNegotiationError> {
        let pending = match (&self.pending_offer, self.state) {
            (Some(p), s) if s != NegotiationState::Idle => p.clone(),
            _ => {
                return Err(err(
                    SdpRule::AnswerWithoutOffer,
                    "RFC 3264 §5",
                    "answer received with no outstanding offer",
                ))
            }
        };
        if sdp.media.len() != pending.media.len() {
            return Err(err(
                SdpRule::MLineCountMismatch,
                "RFC 3264 §6",
                format!(
                    "answer has {} m-lines, offer had {}",
                    sdp.media.len(),
                    pending.media.len()
                ),
            ));
        }
        let m = sdp.audio_media().ok_or_else(|| {
            err(
                SdpRule::NoMedia,
                "RFC 3264 §5.1",
                "answer has no audio media description",
            )
        })?;

        // The answer must select exactly from the codecs we offered.
        let offer_names: Vec<&str> = pending.media[0]
            .rtpmap
            .iter()
            .map(|r| r.encoding_name.as_str())
            .collect();
        let answer_name = m.formats.first().and_then(|&pt| resolve_codec_name(m, pt));
        let answer_name = match answer_name {
            Some(n) if offer_names.contains(&n.as_str()) => n,
            other => {
                return Err(err(
                    SdpRule::AnswerCodecNotInOffer,
                    "RFC 3264 §6",
                    format!(
                        "answer codec {} was not in the offer",
                        other.as_deref().unwrap_or("(none)")
                    ),
                ))
            }
        };
        let chosen = self.codec_by_name(&answer_name).ok_or_else(|| {
            err(
                SdpRule::EmptyCodecIntersection,
                "RFC 3264 §6",
                format!("answer codec {answer_name} unsupported"),
            )
        })?;

        let remote_addr = media_connection_addr(sdp, m);
        let reachable = is_reachable(&remote_addr, m.port);
        // Map the answerer's direction to ours (mirror).
        let our_dir = m.direction.reverse();
        let send =
            matches!(our_dir, MediaDirection::SendRecv | MediaDirection::SendOnly) && reachable;
        let receive = matches!(our_dir, MediaDirection::SendRecv | MediaDirection::RecvOnly);

        let negotiated = NegotiatedMedia {
            remote: NetAddr::new(remote_addr, m.port),
            codec: chosen,
            direction: our_dir,
            send,
            receive,
        };
        self.negotiated = Some(negotiated.clone());

        if reliable {
            self.pending_offer = None;
            self.state = if send {
                NegotiationState::Committed
            } else {
                NegotiationState::Held
            };
        } else {
            self.state = NegotiationState::Early; // provisional; a later final answer may supersede
        }
        Ok(negotiated)
    }

    pub fn negotiated(&self) -> Option<&NegotiatedMedia> {
        self.negotiated.as_ref()
    }

    pub fn state(&self) -> NegotiationState {
        self.state
    }

    /// Re-serialise an [`Sdp`] to wire text — convenience for callers that send
    /// the offer/answer over SIP.
    pub fn to_wire(sdp: &Sdp) -> String {
        build_sdp(sdp)
    }
}

/// Signals carried by a forked branch's provisional response (RFC 5009).
pub struct ProvisionalSignals {
    pub has_sdp: bool,
    /// Value of the P-Early-Media header on the provisional, if present.
    pub p_early_media: Option<MediaDirection>,
    pub reliable: bool,
}

/// RFC 5009 early-media gate: a forked branch's provisional may switch media on
/// only when it carries SDP and is authorized to send early media
/// (`P-Early-Media: sendrecv|sendonly`). Absent/`inactive`/`recvonly` → not
/// authorized.
pub fn is_early_media_authorized(has_sdp: bool, p_early_media: Option<MediaDirection>) -> bool {
    has_sdp
        && matches!(
            p_early_media,
            Some(MediaDirection::SendRecv) | Some(MediaDirection::SendOnly)
        )
}
