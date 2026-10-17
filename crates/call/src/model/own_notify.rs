//! This stack's own NOTIFY on a dialog that has no final yet, and the NOTIFYs
//! and BYE of this stack held behind it. A request with a lower CSeq that
//! arrives after a higher one is refused (RFC 3261 §12.2.2), so under loss a
//! retransmitted NOTIFY overtaken by this stack's next NOTIFY or BYE on the
//! dialog would be lost for good: that request waits for the NOTIFY's final,
//! at most [`OwnNotify::ANSWER_CAP_MS`] after the NOTIFY left.

use serde::{Deserialize, Serialize};
use sip_retransmit::timers::T1;

/// The NOTIFY this stack sent on a dialog that awaits its final, and what waits
/// behind it, in the order it was asked for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnNotify {
    /// The NOTIFY's CSeq number: the final that carries it answers it.
    pub cseq: i64,
    /// When it left: the wait behind it ends [`Self::ANSWER_CAP_MS`] later.
    pub sent_at_ms: i64,
    /// The requests waiting, oldest first.
    #[serde(default)]
    pub held: Vec<HeldRequest>,
}

impl OwnNotify {
    /// 8·T1: the NOTIFY's last retransmission inside it leaves at 7·T1 (RFC
    /// 3261 §17.1.2.2), so a final that has not come by then is read as lost.
    pub const ANSWER_CAP_MS: i64 = 8 * T1 as i64;

    /// When the requests behind the NOTIFY leave though it is unanswered.
    pub fn cap_at_ms(&self) -> i64 {
        self.sent_at_ms + Self::ANSWER_CAP_MS
    }
}

/// A request of this stack asked for while a NOTIFY ahead of it on its dialog
/// had no final, as what it says: it is composed when it leaves, so it takes
/// the dialog's next CSeq then, after any request sent meanwhile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldRequest {
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
