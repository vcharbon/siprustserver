//! The answer against its offer (RFC 3264 §6, §8): one finding per answer.

use sip_message::sdp_doc::{self, SdpDirection};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::doc::stream_row;
use super::reading::Reading;
use super::records::Answered;

/// The finding shape every rule that judges an ANSWER produces: the answerer is
/// charged, on the offer's transaction, anchored at the answer it sent.
fn answer_finding(rule: RuleId, a: &Answered<'_>, decision: Decision) -> Finding {
    Finding {
        rule,
        emitter: a.src.to_string(),
        taker: a.dst.to_string(),
        cseq: a.offer_cseq,
        relayed: false,
        anchor: a.msg,
        decision,
    }
}

/// **RFC 3264 §6 — an accepted stream keeps the offer's media type AND
/// transport protocol.** The answer holds one `m=` line per offered stream, in
/// the offer's order; the `m=` line the answerer sends back for stream *i*
/// re-states that stream's `<media>` and `<proto>` — rejecting it means port 0,
/// never re-typing it. A re-typed or re-transported stream leaves the two ends
/// sending incompatible media on the same slot.
///
/// Charges the ANSWERER, on every round it closes and not only the first. One
/// finding per answer, naming every stream it re-typed: the answer is one act,
/// and a table of streams is one disagreement about that table (the
/// [`Evidence::MultipleFinals`] precedent).
pub struct AnswerStreamMatchesOffer;

impl Obligation for AnswerStreamMatchesOffer {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut offered = Vec::new();
            let mut answered = Vec::new();
            for (i, (o, n)) in offer.doc.media.iter().zip(answer.doc.media.iter()).enumerate() {
                if o.r#type == n.r#type && o.transport == n.transport {
                    continue;
                }
                stream_indexes.push(i);
                offered.push(stream_row(o));
                answered.push(stream_row(n));
            }
            let decision = if stream_indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::AnswerStreamRetyped {
                    answer_stream_msg: a.msg,
                    answer_stream_hop: wire.msgs[a.msg].hop,
                    answer_stream_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    stream_indexes,
                    offered,
                    answered,
                })
            };
            out.push(answer_finding(RuleId::AnswerStreamMatchesOffer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3264 §6 — an answer holds exactly one `m=` line per offered stream.**
/// The two ends index streams by position, so a rejected stream keeps its slot
/// at port 0 and an added one is a re-offer's business, never an answer's. A
/// count that differs desyncs the stream table: from that position on, the two
/// ends mean different streams by the same index.
///
/// Charges the ANSWERER, on every round it closes and not only the first.
pub struct AnswerMLineCountMatchesOffer;

impl Obligation for AnswerMLineCountMatchesOffer {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let (offer_m_lines, answer_m_lines) = (offer.doc.media.len(), answer.doc.media.len());
            let decision = if offer_m_lines == answer_m_lines {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::AnswerMLineCountDiffers {
                    m_count_msg: a.msg,
                    m_count_hop: wire.msgs[a.msg].hop,
                    m_count_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    offer_m_lines,
                    answer_m_lines,
                    offered_streams: offer.stream_table(),
                    answered_streams: answer.stream_table(),
                })
            };
            out.push(answer_finding(RuleId::AnswerMLineCountMatchesOffer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3264 §6 — an answer repeats the offer's `t=` line.** The session's
/// time bounds are the offerer's statement and the answer carries them back
/// verbatim; a rewritten `t=` leaves the two ends disagreeing on when the
/// session is active. A description carrying no `t=` line differs from one that
/// carries any.
///
/// Charges the ANSWERER, on every round it closes.
pub struct AnswerTLineEqualsOffer;

impl Obligation for AnswerTLineEqualsOffer {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let decision = if offer.doc.t_line == answer.doc.t_line {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::AnswerTLineDiffers {
                    t_line_msg: a.msg,
                    t_line_hop: wire.msgs[a.msg].hop,
                    t_line_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    offer_t_line: offer.doc.t_line.clone().unwrap_or_default(),
                    answer_t_line: answer.doc.t_line.clone().unwrap_or_default(),
                })
            };
            out.push(answer_finding(RuleId::AnswerTLineEqualsOffer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3264 §6.1 — the answer's stream at each offered position states that
/// position's media type.** Streams pair by index, so an answer that puts video
/// where audio was offered breaks the correlation both ends apply the
/// negotiated media by.
///
/// Charges the ANSWERER, over the positions both descriptions hold — a count
/// mismatch beyond that is [`AnswerMLineCountMatchesOffer`]'s occasion. One
/// finding per answer, naming every mis-typed position: the answer is one act
/// (the [`Evidence::MultipleFinals`] precedent).
pub struct AnswerMediaTypeMatchesOffer;

impl Obligation for AnswerMediaTypeMatchesOffer {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut offered_types = Vec::new();
            let mut answered_types = Vec::new();
            for (i, (o, n)) in offer.doc.media.iter().zip(answer.doc.media.iter()).enumerate() {
                if o.r#type == n.r#type {
                    continue;
                }
                stream_indexes.push(i);
                offered_types.push(o.r#type.clone());
                answered_types.push(n.r#type.clone());
            }
            let decision = if stream_indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::AnswerMediaTypeMismatched {
                    media_type_msg: a.msg,
                    media_type_hop: wire.msgs[a.msg].hop,
                    media_type_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    stream_indexes,
                    offered_types,
                    answered_types,
                })
            };
            out.push(answer_finding(RuleId::AnswerMediaTypeMatchesOffer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// The answer directions RFC 3264 §6.1 admits for an offered one:
/// `sendonly→{recvonly,inactive}`, `recvonly→{sendonly,inactive}`,
/// `inactive→{inactive}`, `sendrecv→{sendrecv,sendonly,recvonly,inactive}`.
fn answer_valid_for_offer(offer: SdpDirection, answer: SdpDirection) -> bool {
    use SdpDirection::*;
    match offer {
        SendOnly => matches!(answer, RecvOnly | Inactive),
        RecvOnly => matches!(answer, SendOnly | Inactive),
        Inactive => matches!(answer, Inactive),
        SendRecv => matches!(answer, SendRecv | SendOnly | RecvOnly | Inactive),
    }
}

/// **RFC 3264 §6.1 — an answer's direction attribute is one the offer's
/// admits.** The pair states who sends: an answer of `sendonly` to `sendonly`
/// leaves both ends transmitting and neither receiving, and an answer of
/// anything but `inactive` to `inactive` claims a stream the offerer disabled.
/// An `m=` block carrying no direction attribute states `sendrecv`, the default
/// §6.1 defines — an absence, never an unknown.
///
/// Charges the ANSWERER, over the positions both descriptions hold. One finding
/// per answer, naming every mis-paired position.
pub struct DirectionPairValid;

impl Obligation for DirectionPairValid {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut offered_directions = Vec::new();
            let mut answered_directions = Vec::new();
            for (i, (o, n)) in offer.doc.media.iter().zip(answer.doc.media.iter()).enumerate() {
                let (od, nd) = (sdp_doc::extract_direction(o), sdp_doc::extract_direction(n));
                if answer_valid_for_offer(od, nd) {
                    continue;
                }
                stream_indexes.push(i);
                offered_directions.push(od.token().to_string());
                answered_directions.push(nd.token().to_string());
            }
            let decision = if stream_indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::DirectionPairInvalid {
                    direction_msg: a.msg,
                    direction_hop: wire.msgs[a.msg].hop,
                    direction_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    stream_indexes,
                    offered_directions,
                    answered_directions,
                })
            };
            out.push(answer_finding(RuleId::DirectionPairValid, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3264 §6 — an answer rejecting a stream still lists a media format.**
/// Rejection is port 0 on an otherwise complete `m=` line: the format list is
/// part of the line's grammar (RFC 4566 §5.14), so a bare `m=audio 0 RTP/AVP`
/// is a description a strict peer refuses rather than the disable it meant.
///
/// Charges the ANSWERER, on every round it closes; the offer decides nothing
/// here, so the answer alone is the occasion. One finding per answer, naming
/// every bare rejection.
pub struct RejectedStreamMinimalAnswer;

impl Obligation for RejectedStreamMinimalAnswer {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((_, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut rejected_rows = Vec::new();
            for (i, m) in answer.doc.media.iter().enumerate() {
                if m.port != Some(0) || !m.formats.is_empty() {
                    continue;
                }
                stream_indexes.push(i);
                rejected_rows.push(stream_row(m));
            }
            let decision = if stream_indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::RejectedStreamWithoutFormat {
                    rejected_stream_msg: a.msg,
                    rejected_stream_hop: wire.msgs[a.msg].hop,
                    rejected_stream_ts_us: wire.msgs[a.msg].at_us,
                    stream_indexes,
                    rejected_rows,
                })
            };
            out.push(answer_finding(RuleId::RejectedStreamMinimalAnswer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3264 §8 — a stream offered at port 0 is answered at port 0.** Port 0
/// is the offerer disabling the stream; the answerer cannot revive it, and a
/// live port there points the offerer's media at a stream it just took down.
///
/// Charges the ANSWERER, over the positions both descriptions hold. An answer
/// `m=` line carrying no readable port states no assignment, so that position
/// settles nothing — an answer whose every disabled position is unreadable is
/// undecidable rather than clean. One finding per answer, naming every revived
/// position.
pub struct ZeroPortPropagation;

impl Obligation for ZeroPortPropagation {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut answered_ports = Vec::new();
            let (mut disabled, mut unreadable) = (0u32, 0u32);
            for (i, (o, n)) in offer.doc.media.iter().zip(answer.doc.media.iter()).enumerate() {
                if o.port != Some(0) {
                    continue;
                }
                disabled += 1;
                match n.port {
                    None => unreadable += 1,
                    Some(0) => {}
                    Some(p) => {
                        stream_indexes.push(i);
                        answered_ports.push(p);
                    }
                }
            }
            let decision = if !stream_indexes.is_empty() {
                Decision::Violated(Evidence::ZeroPortResurrected {
                    zero_port_msg: a.msg,
                    zero_port_hop: wire.msgs[a.msg].hop,
                    zero_port_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    stream_indexes,
                    answered_ports,
                })
            } else if disabled > 0 && unreadable == disabled {
                Decision::Undecidable(
                    "the answer states no readable port for any stream the offer disabled",
                )
            } else {
                Decision::Compliant
            };
            out.push(answer_finding(RuleId::ZeroPortPropagation, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}
