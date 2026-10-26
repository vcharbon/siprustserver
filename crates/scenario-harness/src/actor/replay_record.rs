//! The replay record of a call: what each `ObserveFinal`/`ExpectFinal`
//! observed beside its expectation, and what the reactor did that no script
//! asked for. Divergence is data, never a verdict.

/// One `ObserveFinal`/`ExpectFinal` outcome: the capture-declared expectation
/// beside what actually arrived. Divergence is data, never a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFinal {
    pub key: u32,
    pub expected: Option<u16>,
    pub observed: u16,
}

/// One entry of the replay record — an observed final beside its expectation,
/// a request the reactor (not the script) serviced on a `Scripted` actor, or a
/// policy-blessed substitution (ADR-0024 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayEntry {
    Final(RecordedFinal),
    ServicedStray {
        leg: &'static str,
        method: String,
        action: &'static str,
    },
    /// An accepted-delta policy blessed a non-matching inbound as satisfying
    /// the due scripted expectation: `step` is the goal-cursor index of that
    /// expectation on `leg`; `expected`/`observed` are the confrontation's
    /// bounded labels (method or status); `rule` the policy's rule name. An
    /// acceptance is never silent — the report side classifies on this entry.
    AcceptedDelta {
        leg: &'static str,
        step: usize,
        expected: String,
        observed: String,
        rule: &'static str,
    },
    /// An inbound a SHARED endpoint's demux could hand to no actor — an initial
    /// INVITE no pending claim owns, a response no member's dialog owns, or a
    /// message for an actor that has already finished. `endpoint` names the UA
    /// it arrived on, `detail` the message. Counted and recorded, never
    /// silently dropped.
    UnclaimedInbound {
        endpoint: String,
        detail: String,
    },
    /// A final RETRANSMITTED on `leg` while this endpoint holds its ACK (a
    /// declared delayed automatic): the peer's own RFC 3261 §13.3.1.4
    /// re-passing, absorbed by the scheduled ACK. ONE entry per retransmission,
    /// so a run states how many the hold provoked instead of hiding them.
    HeldFinalRetransmitted {
        leg: &'static str,
        status: u16,
        cseq_method: String,
        cseq: u32,
    },
    /// A reception goal's [`super::goals::BodyExpect`] was not met by the
    /// message that otherwise satisfied the goal: divergence DATA, never a step
    /// failure — the run continues and the report side classifies the record.
    /// `status` is `Some` for a response (with `method` its CSeq method), `None`
    /// for a request (`method` the request's own, `initial` its dialog axis).
    BodyExpectMiss {
        leg: &'static str,
        step: usize,
        expected: &'static str,
        body_len: usize,
        body_is_sdp: bool,
        status: Option<u16>,
        method: String,
        initial: bool,
    },
}
