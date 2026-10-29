//! The dialling facts of the dialog-creating INVITE a leg sent
//! ([`OutboundInvite`]): what each dialog a response to it creates starts from
//! (RFC 3261 §12.1.2), and how its client transaction ended. The INVITE's
//! number and image are its request record's
//! ([`RequestRecord`](super::RequestRecord)), which holds these facts.

use serde::{Deserialize, Serialize};

/// The INVITE this stack sent to open a leg. Every dialog its responses
/// create carries `our_tag`, its From-tag, and starts its own sequence at the
/// INVITE's CSeq (§12.1.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundInvite {
    pub our_tag: String,
    /// When the INVITE left, epoch ms: the start of its client transaction,
    /// whose bound every deadline of the leg's setup stays inside. Past an
    /// admission its turn awaited it is later than the turn's instant, which
    /// the turn's CDR events keep.
    pub sent_at_ms: i64,
    /// The final the INVITE took: the first final response's status, or 408
    /// for a transaction this stack gave up on (§8.1.3.1). `None` while it
    /// has none.
    pub final_received: Option<u16>,
    /// When the final a response brought arrived, epoch ms; `None` while it
    /// has none, and for a final this stack gave itself.
    #[serde(default)]
    pub final_at_ms: Option<i64>,
    /// This stack sent a CANCEL of the INVITE (§9.1).
    pub cancel_sent: bool,
    /// The headers that CANCEL stated beyond the ones it echoes from the
    /// INVITE (a `Reason`, RFC 3326), so a node that takes the call over
    /// rebuilds the same CANCEL.
    pub cancel_headers: Vec<(String, String)>,
}

impl OutboundInvite {
    /// The INVITE `our_tag` names, sent at `sent_at_ms`, with no final and no
    /// CANCEL yet.
    pub fn new(our_tag: impl Into<String>, sent_at_ms: i64) -> Self {
        OutboundInvite {
            our_tag: our_tag.into(),
            sent_at_ms,
            final_received: None,
            final_at_ms: None,
            cancel_sent: false,
            cancel_headers: Vec::new(),
        }
    }

    /// Record `status` as the INVITE's final; the first final stands
    /// (§17.1.1.2: the transaction takes one).
    pub fn record_final(&mut self, status: u16) {
        self.final_received.get_or_insert(status);
    }

    /// [`record_final`](Self::record_final) of a final a response brought at
    /// `at_ms`, the instant kept with the first final.
    pub fn record_final_at(&mut self, status: u16, at_ms: i64) {
        if self.final_received.is_none() {
            self.final_received = Some(status);
            self.final_at_ms = Some(at_ms);
        }
    }
}
