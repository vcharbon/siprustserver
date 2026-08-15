//! The per-dialog SDP **offer/answer exchange state machine** (RFC 3261 §13.2 /
//! RFC 3264 §6 + §8 / RFC 4566 §5.2).
//!
//! The rules in [`rfc3264_cross`](super::rfc3264_cross) judge ONE offer/answer
//! pair per agent slot — the first body and the first opposite-direction body
//! after it. That view cannot say *which round* a message belongs to, so it
//! cannot tell a legal answer from a body that has no place in the exchange at
//! all. This module carries the missing state: for every dialog, per agent, it
//! tracks each offer/answer **round** (the transaction that carried the offer,
//! and the message that closed it with the answer) and the **origin stream**
//! (the `o=` line of every session description that agent emitted).
//!
//! Three invariants fall out of that state, one rule each:
//!   - [`AckBodyAfterCompleteOfferAnswerRule`] — an ACK adds a body to a round
//!     that is already closed.
//!   - [`AnswerStreamMatchesOfferRule`] — an answer re-types or re-transports a
//!     stream the offer defined.
//!   - [`SdpOriginContinuityRule`] — an agent's session descriptions stop being
//!     one session.
//!
//! Every rule judges only the messages a slot **sent**, so a finding always
//! names the party at fault, and one wire message is never reported twice (once
//! from each end). Relay slots are skipped ([`slot_is_relay`]): a transparent
//! proxy carries both agents' descriptions on one Call-ID and has no single
//! offer/answer state of its own.
//!
//! SDP is lifted with [`parse_sdp_body`] / [`parse_origin`]; this module never
//! parses wire syntax itself.

use std::collections::HashMap;
use std::sync::Arc;

use layer_harness::{LaneKey, Stamped};

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{
    project_per_dialog, slot_is_relay, AgentSlot, EventKind, OrderedEvent,
};
use crate::rfc_audit::offer_answer::{parse_origin, parse_sdp_body, SdpDoc};
use sip_message::{Method, SipMessage};

// ---------------------------------------------------------------------------
// The state machine
// ---------------------------------------------------------------------------

/// Identity of one offer/answer round inside a dialog slot: the CSeq sequence
/// number and method of the transaction that carried the offer, plus whether
/// THIS slot sent that request. Both ends of a dialog number their requests
/// independently (RFC 3261 §12.2), so the direction is part of the key.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct RoundKey {
    cseq: u32,
    method: Method,
    request_sent_by_slot: bool,
}

/// Where a round stands: an offer is on the table, or an answer has closed it.
#[derive(Clone, Debug)]
struct Round {
    /// The offer's session description.
    offer: SdpDoc,
    /// How the offer reached the wire, for the finding text (e.g. `INVITE (CSeq 3)`).
    offer_label: String,
    /// The message that carried the answer, once the round is closed.
    answered_by: Option<String>,
}

/// A message the walk classifies, reduced to what the rules need.
struct Frame<'a> {
    /// This slot put the message on the wire (as opposed to receiving it).
    sent: bool,
    msg: &'a SipMessage,
    /// The SDP body, when the message carries one that parses as a session
    /// description. A body that is not SDP is not part of the exchange.
    sdp: Option<SdpDoc>,
}

fn frame(ev: &OrderedEvent) -> Frame<'_> {
    let body: &[u8] = match &ev.msg {
        SipMessage::Request(r) => r.body(),
        SipMessage::Response(r) => r.body(),
    };
    Frame { sent: ev.kind == EventKind::Sent, msg: &ev.msg, sdp: parse_sdp_body(body) }
}

/// A human label for the message a finding is about — method or status, plus
/// the CSeq that ties it to its transaction.
fn label(msg: &SipMessage) -> String {
    match msg {
        SipMessage::Request(r) => format!("{} (CSeq {})", r.method(), r.cseq().seq()),
        SipMessage::Response(r) => {
            format!("{} to {} (CSeq {})", r.status(), r.cseq().method(), r.cseq().seq())
        }
    }
}

/// The `m=` lines of a description rendered back as `<type>/<transport>` pairs,
/// so a finding can quote the stream table it is judging.
fn stream_table(doc: &SdpDoc) -> String {
    if doc.media.is_empty() {
        return "no m= line".to_string();
    }
    doc.media
        .iter()
        .map(|m| format!("{}/{}", m.r#type, m.transport))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the walk reports back to a rule at each decision point of the state
/// machine. One walk feeds all three rules; each rule keeps the events it cares
/// about, so the state machine is written once.
enum Transition<'a> {
    /// `ack` carries a session description although `round` was already closed.
    BodyOnClosedRound { sent: bool, ack: &'a SipMessage, body: SdpDoc, round: Round },
    /// `answer` closed `round`; the pair is now comparable.
    RoundAnswered {
        sent: bool,
        answer: &'a SipMessage,
        offer: SdpDoc,
        offer_label: String,
        doc: SdpDoc,
    },
    /// `msg`, sent by this slot, carries a session description whose `o=` line
    /// follows `previous` in the slot's own origin stream. `wire_pos` is the
    /// carrying message's 1-based wire-entry position ([`OrderedEvent::wire_pos`]).
    OriginAdvanced {
        msg: &'a SipMessage,
        wire_pos: Option<usize>,
        body: SdpDoc,
        previous: Option<SdpDoc>,
    },
}

/// Replay one agent slot's ordered stream through the offer/answer state
/// machine, emitting a [`Transition`] at every point a rule can judge.
///
/// A request carrying SDP opens a round on its own transaction; the first
/// non-failure response to that transaction carrying SDP closes it. A request
/// WITHOUT SDP whose 2xx carries SDP is the delayed-offer form (RFC 3261
/// §13.2.1): there the 2xx is the offer and the ACK is the answer.
fn walk<F>(slot: &AgentSlot, mut on: F)
where
    F: FnMut(Transition<'_>),
{
    let mut rounds: HashMap<RoundKey, Round> = HashMap::new();
    // Rounds whose offer arrived in the 2xx — the ACK owes them the answer.
    let mut delayed: HashMap<(u32, bool), (SdpDoc, String)> = HashMap::new();
    // The last session description this slot SENT, for the origin stream.
    let mut last_sent_sdp: Option<SdpDoc> = None;

    for ev in &slot.ordered {
        let f = frame(ev);

        if f.sent {
            if let Some(doc) = f.sdp.clone() {
                on(Transition::OriginAdvanced {
                    msg: f.msg,
                    wire_pos: ev.wire_pos,
                    body: doc.clone(),
                    previous: last_sent_sdp.clone(),
                });
                last_sent_sdp = Some(doc);
            }
        }

        match f.msg {
            SipMessage::Request(req) if req.method() == &Method::Ack => {
                let key = RoundKey {
                    cseq: req.cseq().seq(),
                    method: Method::Invite,
                    request_sent_by_slot: f.sent,
                };
                match (rounds.get(&key), f.sdp.clone()) {
                    // The round already has both halves — this body belongs to
                    // no round at all.
                    (Some(round), Some(body)) if round.answered_by.is_some() => {
                        on(Transition::BodyOnClosedRound {
                            sent: f.sent,
                            ack: f.msg,
                            body,
                            round: round.clone(),
                        });
                    }
                    // Delayed offer: the ACK carries the answer to the 2xx.
                    (_, Some(body)) => {
                        if let Some((offer, offer_label)) =
                            delayed.remove(&(req.cseq().seq(), f.sent))
                        {
                            on(Transition::RoundAnswered {
                                sent: f.sent,
                                answer: f.msg,
                                offer,
                                offer_label,
                                doc: body,
                            });
                        }
                    }
                    (_, None) => {}
                }
            }
            SipMessage::Request(req) => {
                let Some(doc) = f.sdp.clone() else { continue };
                let key = RoundKey {
                    cseq: req.cseq().seq(),
                    method: req.method().clone(),
                    request_sent_by_slot: f.sent,
                };
                // A retransmitted offer re-opens nothing.
                rounds.entry(key).or_insert(Round {
                    offer: doc,
                    offer_label: label(f.msg),
                    answered_by: None,
                });
            }
            SipMessage::Response(resp) => {
                let Some(doc) = f.sdp.clone() else { continue };
                // 100 Trying and failure finals close no round.
                if !(101..300).contains(&resp.status()) {
                    continue;
                }
                // A response travels opposite to its request.
                let request_sent_by_slot = !f.sent;
                let key = RoundKey {
                    cseq: resp.cseq().seq(),
                    method: resp.cseq().method().clone(),
                    request_sent_by_slot,
                };
                match rounds.get_mut(&key) {
                    Some(round) if round.answered_by.is_none() => {
                        round.answered_by = Some(label(f.msg));
                        on(Transition::RoundAnswered {
                            sent: f.sent,
                            answer: f.msg,
                            offer: round.offer.clone(),
                            offer_label: round.offer_label.clone(),
                            doc,
                        });
                    }
                    Some(_) => {}
                    // No offer on this transaction: a 2xx to an INVITE is the
                    // delayed offer, answered in the ACK.
                    None => {
                        if resp.status() >= 200 && resp.cseq().method() == &Method::Invite {
                            delayed
                                .entry((resp.cseq().seq(), request_sent_by_slot))
                                .or_insert((doc, label(f.msg)));
                        }
                    }
                }
            }
        }
    }
}

/// Every non-relay agent slot of every dialog, with its Call-ID.
fn judged_slots(events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(String, AgentSlot)> {
    project_per_dialog(events)
        .into_iter()
        .flat_map(|slice| {
            let call_id = slice.call_id.clone();
            slice
                .per_agent
                .into_iter()
                .filter(|slot| !slot_is_relay(slot))
                .map(move |slot| (call_id.clone(), slot))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// rfc3261.ackBodyAfterCompleteOfferAnswer
// ---------------------------------------------------------------------------

/// **RFC 3261 §13.2.2.4 — an ACK closing a completed offer/answer exchange
/// carries NO body.** When the INVITE carried the offer and a non-failure
/// response carried the answer, the round is over: §13.2.1 forbids a further
/// offer on that INVITE transaction, and the ACK has no answer left to deliver.
/// A body there is either a third session description nobody asked for or a
/// default body leaking out of the sender — both leave the two ends disagreeing
/// on the negotiated media.
pub struct AckBodyAfterCompleteOfferAnswerRule;

impl CrossMessageAuditRule for AckBodyAfterCompleteOfferAnswerRule {
    fn name(&self) -> &'static str {
        "rfc3261.ackBodyAfterCompleteOfferAnswer"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for (call_id, slot) in judged_slots(events) {
            walk(&slot, |t| {
                let Transition::BodyOnClosedRound { sent, ack, body, round } = t else { return };
                if !sent {
                    return; // the emitter's fault, reported on the emitter's lane
                }
                out.push((
                    slot.bind_key.clone(),
                    format!(
                        "Sent {} carrying a session description ({}) although the offer/answer \
                         exchange it acknowledges was already complete — the offer rode {} and \
                         the answer rode {} (callId {call_id}); expected no body on this ACK \
                         (Content-Length: 0) — RFC 3261 §13.2.2.4 / §13.2.1",
                        label(ack),
                        stream_table(&body),
                        round.offer_label,
                        round.answered_by.as_deref().unwrap_or("an earlier response"),
                    ),
                ));
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3264.answerStreamMatchesOffer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6 — an accepted stream keeps the offer's media type AND
/// transport protocol.** The answer holds one `m=` line per offered stream, in
/// the offer's order; the `m=` line the answerer sends back for stream *i*
/// re-states that stream's `<media>` and `<proto>` — rejecting it means port 0,
/// never re-typing it. A re-typed or re-transported stream leaves the two ends
/// sending incompatible media on the same slot. Judged on every round, not only
/// the first.
pub struct AnswerStreamMatchesOfferRule;

impl CrossMessageAuditRule for AnswerStreamMatchesOfferRule {
    fn name(&self) -> &'static str {
        "rfc3264.answerStreamMatchesOffer"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for (call_id, slot) in judged_slots(events) {
            walk(&slot, |t| {
                let Transition::RoundAnswered { sent, answer, offer, offer_label, doc } = t else {
                    return;
                };
                if !sent {
                    return; // the answerer's fault, reported on the answerer's lane
                }
                for (i, (o, a)) in offer.media.iter().zip(doc.media.iter()).enumerate() {
                    if o.r#type == a.r#type && o.transport == a.transport {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent {} answering m=[{i}] with \"{} {} {}\" — the offer in \
                             {offer_label} defined that stream as \"{} {} {}\", so the answer \
                             must keep media type '{}' and proto '{}' (port 0 to reject it), \
                             not '{}'/'{}' (callId {call_id}) — RFC 3264 §6",
                            label(answer),
                            a.r#type,
                            a.port.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string()),
                            a.transport,
                            o.r#type,
                            o.port.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string()),
                            o.transport,
                            o.r#type,
                            o.transport,
                            a.r#type,
                            a.transport,
                        ),
                    ));
                }
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3264.sdpOriginContinuity
// ---------------------------------------------------------------------------

/// **RFC 4566 §5.2 / RFC 3264 §8 — every session description an agent sends in
/// one dialog describes the SAME session.** `o=<username> <sess-id> <sess-version>`
/// identifies it: a subsequent description repeats username and sess-id and only
/// ever raises sess-version. A changed username or sess-id makes the description
/// a different session the peer must treat as unrelated; a lowered version makes
/// it an older revision the peer is entitled to discard.
pub struct SdpOriginContinuityRule;

impl CrossMessageAuditRule for SdpOriginContinuityRule {
    fn name(&self) -> &'static str {
        "rfc3264.sdpOriginContinuity"
    }

    /// **Advisory — the shared SDP fixtures are not per-agent sessions.**
    /// `OFFER_SDP`/`ANSWER_SDP` and their per-test siblings are constants several
    /// agents reuse, and a hand-written re-offer bumps sess-id alongside
    /// sess-version, so the rule fires on fixture data as much as on stack
    /// behaviour. Gating waits on one session per agent in the fixtures.
    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>)> {
        let mut out = Vec::new();
        for (call_id, slot) in judged_slots(events) {
            walk(&slot, |t| {
                let Transition::OriginAdvanced { msg, wire_pos, body, previous } = t else {
                    return;
                };
                let Some(prev) = previous else { return };
                let (Some(now), Some(before)) =
                    (parse_origin(body.raw.as_bytes()), parse_origin(prev.raw.as_bytes()))
                else {
                    return;
                };
                if now.username != before.username || now.session_id != before.session_id {
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent {} with origin \"o={}\" after \"o={}\" in the same dialog — a \
                             later description keeps username '{}' and sess-id '{}' (only \
                             sess-version rises), so this one describes a different session \
                             (callId {call_id}) — RFC 4566 §5.2 / RFC 3264 §8",
                            label(msg),
                            now.raw_origin_line.trim_start_matches("o="),
                            before.raw_origin_line.trim_start_matches("o="),
                            before.username,
                            before.session_id,
                        ),
                        wire_pos,
                    ));
                } else if now.session_version < before.session_version {
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent {} with origin \"o={}\" after \"o={}\" in the same dialog — \
                             sess-version went backwards ({} < {}); expected a version above {} \
                             (callId {call_id}) — RFC 4566 §5.2 / RFC 3264 §8",
                            label(msg),
                            now.raw_origin_line.trim_start_matches("o="),
                            before.raw_origin_line.trim_start_matches("o="),
                            now.session_version,
                            before.session_version,
                            before.session_version,
                        ),
                        wire_pos,
                    ));
                }
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The cross-message rules defined in this module. Aggregated by [`super::rfc_cross_message_rules`].
pub(crate) fn cross_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![
        Arc::new(AckBodyAfterCompleteOfferAnswerRule),
        Arc::new(AnswerStreamMatchesOfferRule),
        Arc::new(SdpOriginContinuityRule),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    const CALLER: &str = "127.0.0.1:5060";
    const CALLEE: &str = "127.0.0.1:5070";

    fn sent(raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: "alice".to_string(),
                to: CALLEE.parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    fn recv(raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: "alice".to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                packet: UdpPacket { raw, src: CALLER.parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    /// A request from the caller's side. `to_tag = None` marks the establishing
    /// INVITE; every later request carries the confirmed tag so it lands in the
    /// same dialog slice.
    fn request(method: &str, cseq: u32, to_tag: Option<&str>, body: Option<&str>) -> Vec<u8> {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
            None => "<sip:bob@127.0.0.1>".to_string(),
        };
        let body = body.unwrap_or("");
        let ctype = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
        let head = format!(
            "{method} sip:bob@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{method}-{cseq}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             {ctype}Content-Length: {}\r\n\r\n",
            body.len(),
        );
        let mut v = head.into_bytes();
        v.extend_from_slice(body.as_bytes());
        v
    }

    fn response(status: u16, cseq: u32, method: &str, body: Option<&str>) -> Vec<u8> {
        let body = body.unwrap_or("");
        let ctype = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
        let head = format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{method}-{cseq}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {ctype}Content-Length: {}\r\n\r\n",
            body.len(),
        );
        let mut v = head.into_bytes();
        v.extend_from_slice(body.as_bytes());
        v
    }

    const AUDIO_OFFER: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";

    const AUDIO_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";

    const T38_OFFER: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=image 27500 udptl t38\r\n";

    const T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n";

    /// The harness default body that must never ride an ACK: a fresh session
    /// (`o=bob 1 1`) offering audio over RTP/AVP.
    const STRAY_ANSWER: &str = "v=0\r\n\
o=bob 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 20000 RTP/AVP 0\r\n";

    /// INVITE(offer) → 180(answer) → 200 → ACK, then a T.38 re-INVITE(offer) →
    /// 200(answer) → ACK. The ACK bodies are the caller's choice.
    fn t38_call(
        first_ack: Option<&str>,
        second_ack: Option<&str>,
    ) -> Vec<Stamped<SignalingNetworkEvent>> {
        vec![
            sent(request("INVITE", 1, None, Some(AUDIO_OFFER)), 0),
            recv(response(180, 1, "INVITE", Some(AUDIO_ANSWER)), 1),
            recv(response(200, 1, "INVITE", None), 2),
            sent(request("ACK", 1, Some("bt"), first_ack), 3),
            sent(request("INVITE", 3, Some("bt"), Some(T38_OFFER)), 4),
            recv(response(200, 3, "INVITE", Some(T38_ANSWER)), 5),
            sent(request("ACK", 3, Some("bt"), second_ack), 6),
        ]
    }

    // ---- ackBodyAfterCompleteOfferAnswer -------------------------------

    #[test]
    fn ack_with_no_body_after_a_complete_round_is_clean() {
        let evs = t38_call(None, None);
        assert!(AckBodyAfterCompleteOfferAnswerRule.check(&evs).is_empty());
    }

    #[test]
    fn ack_carrying_a_body_on_an_answered_round_is_flagged() {
        let evs = t38_call(None, Some(STRAY_ANSWER));
        let f = AckBodyAfterCompleteOfferAnswerRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        let detail = &f[0].1;
        assert!(detail.contains("ACK (CSeq 3)"), "{detail}");
        assert!(detail.contains("audio/RTP/AVP"), "{detail}");
        assert!(detail.contains("INVITE (CSeq 3)"), "{detail}");
        assert!(detail.contains("200 to INVITE (CSeq 3)"), "{detail}");
        assert!(detail.contains("RFC 3261 §13.2.2.4"), "{detail}");
    }

    #[test]
    fn ack_body_on_the_establishing_round_answered_by_a_180_is_flagged() {
        let evs = t38_call(Some(STRAY_ANSWER), None);
        let f = AckBodyAfterCompleteOfferAnswerRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("180 to INVITE (CSeq 1)"), "{}", f[0].1);
    }

    #[test]
    fn delayed_offer_ack_carrying_the_answer_is_clean() {
        // INVITE without SDP → the 2xx holds the offer → the ACK holds the answer.
        let evs = vec![
            sent(request("INVITE", 1, None, None), 0),
            recv(response(200, 1, "INVITE", Some(AUDIO_OFFER)), 1),
            sent(request("ACK", 1, Some("bt"), Some(AUDIO_ANSWER)), 2),
        ];
        assert!(AckBodyAfterCompleteOfferAnswerRule.check(&evs).is_empty());
    }

    // ---- answerStreamMatchesOffer --------------------------------------

    #[test]
    fn answer_keeping_media_type_and_proto_is_clean() {
        // Judged from the ANSWERER's slot: it receives the offer and sends the answer.
        let evs = vec![
            recv(request("INVITE", 1, None, Some(T38_OFFER)), 0),
            sent(response(200, 1, "INVITE", Some(T38_ANSWER)), 1),
            recv(request("ACK", 1, Some("bt"), None), 2),
        ];
        assert!(AnswerStreamMatchesOfferRule.check(&evs).is_empty());
    }

    #[test]
    fn answer_re_typing_the_stream_is_flagged() {
        let evs = vec![
            recv(request("INVITE", 1, None, Some(T38_OFFER)), 0),
            sent(response(200, 1, "INVITE", Some(AUDIO_ANSWER)), 1),
            recv(request("ACK", 1, Some("bt"), None), 2),
        ];
        let f = AnswerStreamMatchesOfferRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        let detail = &f[0].1;
        assert!(detail.contains("m=[0]"), "{detail}");
        assert!(detail.contains("audio 60788 RTP/AVP"), "{detail}");
        assert!(detail.contains("image 27500 udptl"), "{detail}");
        assert!(detail.contains("RFC 3264 §6"), "{detail}");
    }

    #[test]
    fn answer_re_transporting_the_stream_is_flagged() {
        const SAVP_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 60788 RTP/SAVP 8\r\n";
        let evs = vec![
            recv(request("INVITE", 1, None, Some(AUDIO_OFFER)), 0),
            sent(response(200, 1, "INVITE", Some(SAVP_ANSWER)), 1),
            recv(request("ACK", 1, Some("bt"), None), 2),
        ];
        let f = AnswerStreamMatchesOfferRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("proto 'RTP/AVP'"), "{}", f[0].1);
    }

    #[test]
    fn rejected_stream_keeps_type_and_proto_at_port_zero() {
        const REJECTED: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 8\r\n";
        let evs = vec![
            recv(request("INVITE", 1, None, Some(AUDIO_OFFER)), 0),
            sent(response(200, 1, "INVITE", Some(REJECTED)), 1),
            recv(request("ACK", 1, Some("bt"), None), 2),
        ];
        assert!(AnswerStreamMatchesOfferRule.check(&evs).is_empty());
    }

    // ---- sdpOriginContinuity -------------------------------------------

    #[test]
    fn rising_session_version_on_one_origin_is_clean() {
        let evs = t38_call(None, None);
        assert!(SdpOriginContinuityRule.check(&evs).is_empty());
    }

    #[test]
    fn a_foreign_origin_mid_dialog_is_flagged() {
        let evs = t38_call(None, Some(STRAY_ANSWER));
        let f = SdpOriginContinuityRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        let detail = &f[0].1;
        assert!(detail.contains("ACK (CSeq 3)"), "{detail}");
        assert!(detail.contains("o=bob 1 1 IN IP4 127.0.0.1"), "{detail}");
        assert!(detail.contains("o=alice 424242 2 IN IP4 10.0.0.1"), "{detail}");
        assert!(detail.contains("RFC 4566 §5.2"), "{detail}");
    }

    #[test]
    fn foreign_origin_offending_points_at_the_carrying_ack() {
        // Address-keyed bind so the events project into wire entries (the
        // symbolic "alice" bind of the shared helpers yields no positions).
        fn sent_on(raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
            Stamped {
                event: SignalingNetworkEvent::SendCalled {
                    bind_key: "127.0.0.1:5091".to_string(),
                    to: CALLEE.parse().unwrap(),
                    msg: raw,
                },
                seq,
                at_ms: seq,
            }
        }
        fn recv_on(raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
            Stamped {
                event: SignalingNetworkEvent::RecvItem {
                    bind_key: "127.0.0.1:5091".to_string(),
                    disposition: crate::types::RecvDisposition::Delivered,
                    packet: UdpPacket { raw, src: CALLER.parse().unwrap(), arrival_ms: seq },
                },
                seq,
                at_ms: seq,
            }
        }
        let evs = vec![
            sent_on(request("INVITE", 1, None, Some(AUDIO_OFFER)), 0),
            recv_on(response(200, 1, "INVITE", Some(AUDIO_ANSWER)), 1),
            sent_on(request("ACK", 1, Some("bt"), Some(STRAY_ANSWER)), 2),
        ];
        let out = SdpOriginContinuityRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].2, Some(3), "offending = the ACK carrying the foreign origin (entry 3)");
    }

    #[test]
    fn a_lowered_session_version_is_flagged() {
        const STALE_REOFFER: &str = "v=0\r\n\
o=alice 424242 0 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
        let evs = vec![
            sent(request("INVITE", 1, None, Some(AUDIO_OFFER)), 0),
            recv(response(200, 1, "INVITE", Some(AUDIO_ANSWER)), 1),
            sent(request("ACK", 1, Some("bt"), None), 2),
            sent(request("INVITE", 3, Some("bt"), Some(STALE_REOFFER)), 3),
        ];
        let f = SdpOriginContinuityRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("sess-version went backwards (0 < 1)"), "{}", f[0].1);
    }

    #[test]
    fn a_received_peer_origin_never_lands_on_this_slot() {
        // The slot only RECEIVES the peer's descriptions — its own origin
        // stream is empty, so the peer's discontinuity is not its finding.
        let evs = vec![
            recv(request("INVITE", 1, None, Some(AUDIO_OFFER)), 0),
            sent(response(200, 1, "INVITE", Some(AUDIO_ANSWER)), 1),
            recv(request("ACK", 1, Some("bt"), Some(STRAY_ANSWER)), 2),
        ];
        assert!(SdpOriginContinuityRule.check(&evs).is_empty());
    }
}
