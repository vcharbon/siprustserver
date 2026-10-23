//! A subscription this stack is the notifier of on a dialog
//! ([`Subscription`], ADR-0055 §2): a dialog usage of its own (RFC 5057 §3),
//! named by its event package and `id` (RFC 6665 §4.5) — the implicit one a
//! REFER opens (RFC 3515 §2.4.4) among them — with the state its NOTIFYs
//! reported and its NOTIFY awaiting a final. Once terminated, no NOTIFY
//! leaves on it (RFC 6665 §4.2.2).

use serde::{Deserialize, Serialize};
use sip_retransmit::timers::T1;

/// Where a subscription stands (RFC 6665 §4.1.3). `Terminated` is final.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionState {
    Pending,
    Active,
    Terminated,
}

/// One subscription of a dialog this stack sends the NOTIFYs of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    /// The event package, lower-case.
    pub event: String,
    /// The `id` parameter of its `Event`; `None` where it states none.
    pub id: Option<String>,
    pub state: SubscriptionState,
    /// Its latest NOTIFY while that NOTIFY awaits a final.
    pub notify: Option<NotifyInFlight>,
}

impl Subscription {
    /// The subscription `event` and `id` name, before any NOTIFY of it.
    pub fn new(event: &str, id: Option<&str>) -> Self {
        Subscription {
            event: event.to_ascii_lowercase(),
            id: id.map(str::to_string),
            state: SubscriptionState::Pending,
            notify: None,
        }
    }

    /// Whether `event` and `id` name this subscription: the package matched
    /// without case, the `id` exactly (RFC 6665 §8.2.1, §4.5).
    pub fn is_named(&self, event: &str, id: Option<&str>) -> bool {
        self.event.eq_ignore_ascii_case(event) && self.id.as_deref() == id
    }

    pub fn is_terminated(&self) -> bool {
        self.state == SubscriptionState::Terminated
    }

    /// Record the state a NOTIFY of this stack reports; a terminated
    /// subscription stays terminated.
    pub fn report(&mut self, state: SubscriptionState) {
        if !self.is_terminated() {
            self.state = state;
        }
    }
}

/// The NOTIFY of a subscription that awaits its final: what this stack asks
/// for next on the dialog waits for that final, at most
/// [`NotifyInFlight::ANSWER_CAP_MS`] after the NOTIFY left. A request with a
/// lower CSeq that arrives after a higher one is refused (RFC 3261 §12.2.2),
/// so under loss a retransmitted NOTIFY overtaken by the next request would
/// be lost for good.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyInFlight {
    /// The NOTIFY's CSeq number: the final that carries it answers it.
    pub cseq: i64,
    /// When it left.
    pub sent_at_ms: i64,
}

impl NotifyInFlight {
    /// 8·T1: the NOTIFY's last retransmission inside it leaves at 7·T1 (RFC
    /// 3261 §17.1.2.2), so a final that has not come by then is read as lost.
    #[expect(clippy::cast_possible_wrap, reason = "T1 is the constant 500 ms")]
    pub const ANSWER_CAP_MS: i64 = 8 * T1 as i64;

    /// When the requests behind the NOTIFY leave though it is unanswered.
    pub fn cap_at_ms(&self) -> i64 {
        self.sent_at_ms + Self::ANSWER_CAP_MS
    }
}
