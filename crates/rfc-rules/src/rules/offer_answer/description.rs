//! The obligations a description owes on its own bytes, outside any round.

use sip_message::sdp;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::doc::{stream_row, Doc};

/// **RFC 3264 §5-6 / RFC 4566 §5 — a body sent as SDP satisfies the
/// offer/answer grammar.** A real peer 488s a malformed description: no `v=0`
/// session, two sessions in one body, an `o=` sess-id/version that is not a
/// digit string, a stream with no `c=` or no port, a non-positive `a=ptime`.
/// A test UA accepts the body and masks the defect, so the recording is where
/// the offer's own grammar is checked at all.
///
/// **The occasion is one fresh message the endpoint sent DECLARING a
/// description and carrying bytes** — `application/sdp`, or a `multipart/…`
/// body framing an SDP part (RFC 5621 §3.1). The declaration is what says
/// these bytes are a description: a body under another Content-Type is not this
/// rule's to read, and a vantage carrying no head or no body opens no occasion
/// rather than guessing at one. The grammar walk is
/// [`sip_message::sdp::validate_offer_answer_body`]'s — this rule reads a
/// description, it never parses one. Charges the sender that minted the body.
pub struct SdpBodyParseable;

impl Obligation for SdpBodyParseable {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(body) = msg.sdp() else { continue };
            let decision = match sdp::validate_offer_answer_body(body) {
                Ok(()) => Decision::Compliant,
                Err(e) => Decision::Violated(Evidence::SdpBodyRejected {
                    sdp_body_msg: mi,
                    sdp_body_hop: msg.hop,
                    sdp_body_ts_us: msg.at_us,
                    sdp_failure: e.reason,
                }),
            };
            out.push(Finding {
                rule: RuleId::SdpBodyParseable,
                emitter: msg.src.clone(),
                taker: msg.dst.clone(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            });
        }
        out
    }
}

/// **RFC 3264 §6 / §8.4 — a stream is held OR rejected, never both.** Setting
/// the connection address to the unspecified one (`c=IN IP4 0.0.0.0`, the
/// legacy hold idiom) says "keep the stream, send nothing there"; port 0 says
/// "this stream is gone". A description stating both leaves the peer no
/// disposition it can act on, and the two answers it could give differ.
///
/// **The occasion is one fresh description the endpoint sent**, whatever
/// Content-Type declared it — the shape alone identifies a session description,
/// and this rule reads the disposition rather than the format. The applicable
/// `c=` is the stream's own where it states one, the session's otherwise
/// (§5.7), so a block overriding an unspecified session address with a real one
/// is not held at all.
///
/// **One finding per DESCRIPTION**, naming every stream at fault: writing one
/// body with contradictory dispositions is one act. Charges the sender; the
/// live row scopes it to the UAC.
pub struct C0PortNonZero;

impl Obligation for C0PortNonZero {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(carried) = Doc::of(msg) else { continue };
            let session_c = carried.doc.c_line.as_deref();
            let session_held = session_c.is_some_and(sdp::c_line_is_unspecified);
            let (mut indexes, mut streams, mut c_lines) = (Vec::new(), Vec::new(), Vec::new());
            for (i, media) in carried.doc.media.iter().enumerate() {
                let applicable = media.c_line.as_deref().or(session_c);
                let held = match media.c_line.as_deref() {
                    Some(own) => sdp::c_line_is_unspecified(own),
                    None => session_held,
                };
                if held && media.port == Some(0) {
                    indexes.push(i);
                    streams.push(stream_row(media));
                    c_lines.push(applicable.unwrap_or_default().to_string());
                }
            }
            let decision = if indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::HeldAndRejectedStreams {
                    held_stream_msg: mi,
                    held_stream_hop: msg.hop,
                    held_stream_ts_us: msg.at_us,
                    held_stream_indexes: indexes,
                    held_streams: streams,
                    held_c_lines: c_lines,
                })
            };
            out.push(Finding {
                rule: RuleId::C0PortNonZero,
                emitter: msg.src.clone(),
                taker: msg.dst.clone(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            });
        }
        out
    }
}
