//! The offer a responder made on a dialog this stack dialled that still awaits
//! its answer from this side (RFC 3264 §4): the INVITE this stack sent carried
//! none, so the responder's first description is the offer, and the message
//! that answers it is fixed by where that description rode.

use serde::{Deserialize, Serialize};

use super::provisional::RSeq;

/// The answer one dialog owes its responder, and which message carries it.
/// One per dialog: RFC 3264 has one offer outstanding at a time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwedAnswer {
    /// The 2xx carried the offer (a delayed offer): the ACK whose CSeq is
    /// `ack_cseq` carries the answer (RFC 3261 §13.2.2.4). `ack_cseq` is in
    /// the acknowledging peer's own sequence space.
    InAck {
        ack_cseq: i64,
        #[serde(with = "serde_bytes")]
        offer: Vec<u8>,
    },
    /// The reliable provisional `rseq` carried the offer: its PRACK carries
    /// the answer (RFC 3262 §5).
    InPrack {
        rseq: RSeq,
        #[serde(with = "serde_bytes")]
        offer: Vec<u8>,
    },
    /// The reliable provisional `rseq` carried the offer and was still
    /// unPRACKed when a 2xx confirmed the dialog: its PRACK waits for the
    /// ACK and carries the answer that ACK gives.
    DeferredToAck {
        rseq: RSeq,
        #[serde(with = "serde_bytes")]
        offer: Vec<u8>,
    },
}

impl OwedAnswer {
    /// The offer a PRACK of provisional `rseq` answers: the one it carried,
    /// whether its PRACK leaves now or with the ACK.
    pub fn prack_offer(&self, rseq: RSeq) -> Option<&[u8]> {
        match self {
            OwedAnswer::InPrack { rseq: r, offer }
            | OwedAnswer::DeferredToAck { rseq: r, offer }
                if *r == rseq =>
            {
                Some(offer)
            }
            _ => None,
        }
    }
}
