//! A request of this stack waiting on a dialog ([`QueuedRequest`]): queued
//! behind its own NOTIFY in flight there, or a BYE behind the ACK of its 2xx.

use serde::{Deserialize, Serialize};

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
