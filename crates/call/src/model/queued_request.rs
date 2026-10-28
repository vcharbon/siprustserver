//! A request of this stack waiting on a dialog ([`Queued`]): what it says
//! ([`QueuedRequest`]) and what it waits behind ([`Behind`]), this stack's
//! own NOTIFY in flight there or the ACK of its 2xx.

use serde::{Deserialize, Serialize};

/// What a queued request waits behind on its dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Behind {
    /// The ACK of this stack's 2xx to the dialog's creating INVITE, or that
    /// 2xx's give-up (RFC 3261 §15): a BYE, held while the 2xx is.
    AnswerAck,
    /// This stack's own NOTIFY in flight on the dialog, so loss never
    /// delivers its requests out of CSeq order (RFC 3261 §12.2.2).
    OwnNotify,
}

/// A request of this stack on a dialog's queue and what it waits behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    pub behind: Behind,
    pub request: QueuedRequest,
}

/// A request of this stack asked for while something ahead of it on its
/// dialog has to settle first, as what it says: it is composed when it
/// leaves, so it takes the dialog's next CSeq then, after any request sent
/// meanwhile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedRequest {
    /// A NOTIFY: once it leaves, what follows waits for its final.
    Notify {
        event: String,
        subscription_state: String,
        content_type: Option<String>,
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    /// A BYE: the `Reason` it states, the lines of the peer's it restates
    /// (`(name, value)`, in order), its body, and its `Max-Forwards`.
    Bye {
        reason: Option<String>,
        headers: Vec<(String, String)>,
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
        content_type: Option<String>,
        max_forwards: Option<u32>,
    },
}
