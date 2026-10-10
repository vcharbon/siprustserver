//! The offer/answer family: the RFC 3261 §13.2 / RFC 3264 obligations that
//! need to know WHICH ROUND a session description belongs to, and the one
//! reading of the wire they share.
//!
//! **The reading is a per-AGENT state machine, and every message feeds two of
//! them.** A description is one endpoint's emission and the other's arrival, so
//! the walk absorbs each message twice — once as `src`, once as `dst` — and
//! each agent keeps its own rounds. A round opens when a request carries a
//! description and closes when the first non-failure response to that
//! transaction carries one; a request WITHOUT a description whose 2xx carries
//! one is the delayed-offer form (§13.2.1), where the 2xx is the offer and the
//! ACK is the answer.
//!
//! **The round is keyed by the transaction AND the direction; the ANSWER is
//! keyed by the early dialog.** Both ends of a dialog number their requests
//! independently (§12.2), so which end sent the offer is part of the round's
//! identity; two forks answering one INVITE are two answers to one offer, so
//! the To tag partitions the closures (the [`super::prack`] precedent).
//! A second description INSIDE one partition closes nothing: the round keeps
//! the answer the peer acted on, and a second BINDING one is its own occasion —
//! [`SecondAnswerRepeatsTheFirst`]'s.
//!
//! **Every occasion here judges what an agent SENT**, so a finding always names
//! the party at fault and one wire message is never judged twice — once from
//! each end. What a message the agent merely TOOK does is the peer's occasion,
//! on the peer's own slot.
//!
//! **A hop's exemption is the CONSUMER's.** Whether a lane merely relays a
//! stream is a property of that lane across the whole recording, not of the
//! message in hand: a transparent proxy carries both agents' descriptions on
//! one Call-ID and has no offer/answer state of its own. These rules leave
//! `relayed` false and the consumer skips the lanes it knows to be relays — the
//! [`super::final_response`] precedent.
//!
//! **A body-less vantage decides nothing.** [`Msg::body`](crate::wire::Msg::body)
//! is `None` where the vantage did not carry the bytes; a rule that needs a body
//! fact there returns `Undecidable`, never a guess. `Some(&[])` is the opposite —
//! the message demonstrably carried no body.
//!
//! **Two of the obligations here need no round at all** —
//! [`SdpBodyParseable`] and [`C0PortNonZero`] judge ONE description on its own
//! bytes — and they still belong to this family: RFC 3264 is one family, every
//! `sdp_doc` reading in this crate is here, and they charge the sender exactly
//! as every round rule does.
//!
//! SDP grammar is [`sip_message::sdp_doc`]'s and [`sip_message::sdp`]'s: this
//! module reads a description, it never parses one.

mod answer;
mod description;
mod doc;
mod reading;
mod records;
mod revision;
mod round;

pub use answer::{
    AnswerMLineCountMatchesOffer, AnswerMediaTypeMatchesOffer, AnswerStreamMatchesOffer,
    AnswerTLineEqualsOffer, DirectionPairValid, RejectedStreamMinimalAnswer, ZeroPortPropagation,
};
pub use description::{C0PortNonZero, SdpBodyParseable};
pub use revision::{PayloadTypeMappingStable, ReOfferMLineCountMonotonic, SdpOriginContinuity};
pub use round::{
    AckBodyAfterCompleteOfferAnswer, DelayedOfferAnsweredInAck, Final2xxAnswersTheOffer,
    NoNewOfferWhileOfferPending, SecondAnswerRepeatsTheFirst,
};

crate::registry::rules! {
    /// RFC 3261 §13.2.2.4: an ACK closing a completed offer/answer exchange carries no body — the
    /// round has both halves and the ACK has no answer left to deliver. Charges the ACK's sender.
    AckBodyAfterCompleteOfferAnswer = "ack-body-after-complete-offer-answer", relayable;
    /// RFC 3261 §13.2.1 / RFC 3264 §6: the non-failure final to a request that carried an offer
    /// carries the answer, unless a reliable provisional already stated one on that dialog. Charges
    /// the answerer.
    Final2xxAnswersTheOffer = "final-2xx-answers-the-offer", relayable;
    /// RFC 3261 §13.2.2.4 / §13.2.1: an ACK to a 2xx that carried a delayed offer carries the
    /// answer. Charges the ACK's sender.
    DelayedOfferAnsweredInAck = "delayed-offer-answered-in-ack";
    /// RFC 3261 §13.2.1: one dialog carries ONE answer — a later description on that dialog
    /// re-states the transport plan the first stated, never another one. Charges the answerer.
    SecondAnswerRepeatsTheFirst = "second-answer-repeats-the-first";
    /// RFC 3264 §6: the `m=` line an answer sends back for an offered stream re-states that
    /// stream's media type and transport — rejecting it means port 0, never re-typing it. Charges
    /// the answerer.
    AnswerStreamMatchesOffer = "answer-stream-matches-offer";
    /// RFC 4566 §5.2 / RFC 3264 §8: every session description an agent sends on one call keeps the
    /// `o=` identity and moves `sess-version` by exactly what changed. Charges the description's
    /// sender.
    SdpOriginContinuity = "sdp-origin-continuity";
    /// RFC 3264 §5: an agent holds one offer of its own outstanding at a time on a call — the next
    /// waits for the answer to the last. Charges the offer's sender.
    NoNewOfferWhileOfferPending = "no-new-offer-while-offer-pending";
    /// RFC 3264 §6: an answer holds exactly one `m=` line per offered stream — a rejected stream
    /// keeps its slot at port 0, it never disappears. Charges the answerer.
    AnswerMLineCountMatchesOffer = "answer-m-line-count-matches-offer";
    /// RFC 3264 §6: an answer repeats the offer's `t=` line — the two ends state one session time.
    /// Charges the answerer.
    AnswerTLineEqualsOffer = "answer-t-line-equals-offer";
    /// RFC 3264 §6.1: the answer's `m=` line at each offered position states that position's media
    /// type — the per-stream pairing both ends index on. Charges the answerer.
    AnswerMediaTypeMatchesOffer = "answer-media-type-matches-offer";
    /// RFC 3264 §6.1: an answer's direction attribute is one the offer's admits — `sendonly` draws
    /// `recvonly`/`inactive`, `inactive` draws `inactive`. Charges the answerer.
    DirectionPairValid = "direction-pair-valid";
    /// RFC 3264 §6: an answer rejecting a stream still lists a media format on its `m=` line — a
    /// bare `m=audio 0 RTP/AVP` is no description at all. Charges the answerer.
    RejectedStreamMinimalAnswer = "rejected-stream-minimal-answer";
    /// RFC 3264 §8: an agent's later description on a call carries at least as many `m=` lines as
    /// the one before — a removed stream keeps its slot at port 0. Charges the description's
    /// sender.
    ReOfferMLineCountMonotonic = "re-offer-m-line-count-monotonic";
    /// RFC 3264 §8: a stream offered at port 0 is answered at port 0 — a disabled stream stays
    /// disabled. Charges the answerer.
    ZeroPortPropagation = "zero-port-propagation";
    /// RFC 3264 §8.3.2: once an agent's description binds a payload type to an encoding, its later
    /// descriptions on that call keep the binding — the peer caches it. Charges the description's
    /// sender.
    PayloadTypeMappingStable = "payload-type-mapping-stable", relayable;
    /// RFC 3264 §5-6 / RFC 4566 §5: a body sent as `application/sdp` satisfies the offer/answer
    /// grammar. Charges the sender that minted it.
    SdpBodyParseable = "sdp-body-parseable";
    /// RFC 3264 §6 / §8.4: a stream held at `c=0.0.0.0` is not ALSO rejected at port 0 — the two
    /// together state no disposition a peer can act on. Charges the sender.
    C0PortNonZero = "c0-port-non-zero";
}

#[cfg(test)]
mod tests;
