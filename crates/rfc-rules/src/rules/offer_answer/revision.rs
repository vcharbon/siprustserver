//! An agent's later descriptions on a call against its earlier ones (RFC 3264 §8,
//! RFC 4566 §5.2).

use sip_message::sdp_doc;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::reading::Reading;
use super::records::OriginStep;

/// **RFC 4566 §5.2 / RFC 3264 §8 — every session description an agent sends on
/// one call describes the SAME session, and its `sess-version` counts the
/// revisions.**
/// `o=<username> <sess-id> <sess-version> <nettype> <addrtype> <address>`
/// identifies it: a later description repeats all five identity
/// fields and raises `sess-version` by exactly one when the rest of the
/// description changed. A byte-identical description may keep its version or
/// raise it by one: §8 only has an unchanged version promise an identical
/// description. A changed identity makes the description a different session
/// the peer must treat as unrelated; a version that does not track the changes
/// leaves the peer unable to tell a re-offer from a repeat.
///
/// Charges the description's SENDER, against its own previous description on
/// that call. A description whose `o=` line no reader accepts settles nothing —
/// the stream's predecessor is the last one that did.
pub struct SdpOriginContinuity;

impl Obligation for SdpOriginContinuity {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        let head = |s: &OriginStep<'_>, decision| Finding {
            rule: RuleId::SdpOriginContinuity,
            emitter: s.src.to_string(),
            taker: s.dst.to_string(),
            cseq: s.cseq,
            relayed: false,
            anchor: s.msg,
            decision,
        };
        for s in &seen.unreadable_origins {
            out.push(head(s, Decision::Undecidable("the description carries no readable o= line")));
        }
        for s in &seen.origin_steps {
            let (Some(now), Some(before)) = (
                seen.docs[s.msg].as_ref().and_then(|d| d.origin.as_ref()),
                seen.docs[s.prior].as_ref().and_then(|d| d.origin.as_ref()),
            ) else {
                continue;
            };
            let same_session = before.identifies_same_session(now);
            let body_changed = before.body_excluding_origin != now.body_excluding_origin;
            let delta = i128::from(now.session_version) - i128::from(before.session_version);
            let ok = same_session && (delta == 1 || (delta == 0 && !body_changed));
            let decision = if ok {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::SdpOriginDiverged {
                    origin_msg: s.msg,
                    origin_hop: wire.msgs[s.msg].hop,
                    origin_ts_us: wire.msgs[s.msg].at_us,
                    prior_origin_msg: s.prior,
                    origin_line: now.raw_origin_line.clone(),
                    prior_origin_line: before.raw_origin_line.clone(),
                    same_session,
                    body_changed,
                    session_version: now.session_version,
                    prior_session_version: before.session_version,
                })
            };
            out.push(head(s, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3264 §8 — an agent's later description carries at least as many `m=`
/// lines as the one before it.** A stream is removed by keeping its slot at
/// port 0, and a new one is appended BELOW the existing ones, so the stream
/// table only ever grows. A shorter table renumbers every stream below the
/// dropped one, and the peer applies the answer to the wrong streams.
///
/// Charges the description's SENDER, against its own previous description on
/// that call — answers included, since the table is the agent's whatever
/// message carries it. A description this vantage carried no body for is no
/// step of the stream at all.
pub struct ReOfferMLineCountMonotonic;

impl Obligation for ReOfferMLineCountMonotonic {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for stream in &seen.sent_streams {
            for step in stream.windows(2) {
                let (prior, msg) = (step[0], step[1]);
                let (Some(before), Some(now)) =
                    (seen.docs[prior].as_ref(), seen.docs[msg].as_ref())
                else {
                    continue;
                };
                let (prior_m_lines, m_lines) = (before.doc.media.len(), now.doc.media.len());
                let decision = if m_lines >= prior_m_lines {
                    Decision::Compliant
                } else {
                    Decision::Violated(Evidence::ReOfferStreamsDropped {
                        re_offer_msg: msg,
                        re_offer_hop: wire.msgs[msg].hop,
                        re_offer_ts_us: wire.msgs[msg].at_us,
                        prior_offer_msg: prior,
                        m_lines,
                        prior_m_lines,
                    })
                };
                out.push(Finding {
                    rule: RuleId::ReOfferMLineCountMonotonic,
                    emitter: wire.msgs[msg].src.clone(),
                    taker: wire.msgs[msg].dst.clone(),
                    cseq: wire.msgs[msg].cseq,
                    relayed: false, // a relay lane is the consumer's to skip
                    anchor: msg,
                    decision,
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// True iff two `a=rtpmap` encodings name ONE binding, read as the media type
/// under judgement spells them: the case-insensitive MIME subtype comparison of
/// RFC 4566 §6, over the canonical form [`sdp_doc::canonical_rtpmap`] gives each.
fn same_binding(media_type: &str, one: &str, other: &str) -> bool {
    sdp_doc::canonical_rtpmap(media_type, one)
        .eq_ignore_ascii_case(&sdp_doc::canonical_rtpmap(media_type, other))
}

/// **RFC 3264 §8.3.2 — a payload type an agent has bound to an encoding keeps
/// that encoding for the whole call.** The peer caches `a=rtpmap:<pt> <enc>`
/// and decodes by the cached binding, so re-binding a payload type — in a later
/// description or in a further `m=` block of the same one — decodes the
/// sender's media as the wrong codec. Two spellings of one binding are not a
/// re-bind (RFC 4566 §6): encoding names are MIME subtypes and compare
/// case-insensitively, and on an audio stream an absent channel count is one,
/// so `pcma`/`PCMA` and `PCMA/8000`/`PCMA/8000/1` each name a single binding.
/// The finding quotes the encodings the wire stated.
///
/// Charges the description's SENDER, against the bindings its own earlier
/// descriptions on that call stated. Every description it sends is an occasion,
/// the first included: the binding a description makes in one `m=` block binds
/// the rest of it too. One finding per description, naming every re-bound
/// payload type.
pub struct PayloadTypeMappingStable;

impl Obligation for PayloadTypeMappingStable {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for stream in &seen.sent_streams {
            // Payload type → the encoding text this agent first bound it to.
            let mut bound: Vec<(String, String)> = Vec::new();
            for &mi in stream {
                let Some(doc) = seen.docs[mi].as_ref() else { continue };
                let mut payload_types = Vec::new();
                let mut prior_encodings = Vec::new();
                let mut encodings = Vec::new();
                for media in &doc.doc.media {
                    for (pt, enc) in sdp_doc::extract_rtpmaps(media) {
                        let first = bound.iter().find(|(p, _)| *p == pt).map(|(_, e)| e.clone());
                        match first {
                            None => bound.push((pt, enc)),
                            Some(prev) if !same_binding(&media.r#type, &prev, &enc) => {
                                payload_types.push(pt);
                                prior_encodings.push(prev);
                                encodings.push(enc);
                            }
                            Some(_) => {}
                        }
                    }
                }
                let decision = if payload_types.is_empty() {
                    Decision::Compliant
                } else {
                    Decision::Violated(Evidence::PayloadTypeRemapped {
                        payload_type_msg: mi,
                        payload_type_hop: wire.msgs[mi].hop,
                        payload_type_ts_us: wire.msgs[mi].at_us,
                        payload_types,
                        prior_encodings,
                        encodings,
                    })
                };
                out.push(Finding {
                    rule: RuleId::PayloadTypeMappingStable,
                    emitter: wire.msgs[mi].src.clone(),
                    taker: wire.msgs[mi].dst.clone(),
                    cseq: wire.msgs[mi].cseq,
                    relayed: false, // a relay lane is the consumer's to skip
                    anchor: mi,
                    decision,
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}
