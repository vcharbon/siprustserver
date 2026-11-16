//! The responses an expect read as its own transaction's (RFC 3261 §17.1.3),
//! for 64·T1 after the read: a byte-identical copy that another
//! transaction's expect reads later, such as a 2xx retransmission while the
//! UAC holds back its ACK (§13.3.1.4), tells the UAC core nothing new. The
//! record is kept at the read, never at the arrival, so a copy set aside
//! unread is never taken for a repeat of itself.

use std::sync::Mutex;
use std::time::Duration;

use sip_message::SipResponse;
use tokio::time::Instant;

/// 64·T1: how long a UAS retransmits its 2xx (§13.3.1.4, Timer H's span).
const RETRANSMIT_SPAN: Duration = Duration::from_secs(32);

/// The responses read, each as its bytes and when it is let go.
#[derive(Default)]
pub(super) struct ReadResponses {
    read: Mutex<Vec<(Box<[u8]>, Instant)>>,
}

impl ReadResponses {
    /// Note `r`, read as its transaction's response.
    pub(super) fn note(&self, r: &SipResponse) {
        let now = Instant::now();
        let mut read = self.read.lock().unwrap();
        read.retain(|(_, until)| *until > now);
        read.push((r.image().as_ref().into(), now + RETRANSMIT_SPAN));
    }

    /// Whether `r` is byte-identical to a response read within 64·T1.
    pub(super) fn holds(&self, r: &SipResponse) -> bool {
        let now = Instant::now();
        let read = self.read.lock().unwrap();
        read.iter().any(|(bytes, until)| *until > now && **bytes == *r.image().as_ref())
    }
}
