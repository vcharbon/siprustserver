//! The requests the proxy drops unanswered for want of an alive target
//! (ADR-0050 §5), by what they were.

use sip_message::{Method, SipRequest};

use super::ProxyMetrics;

/// What a request dropped for want of an alive target was, for
/// `sip_proxy_no_target_dropped_total{kind}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTargetDrop {
    /// A request inside a dialog: routed by its stickiness cookie, or met
    /// before the proxy's first worker view.
    InDialog,
    /// A CANCEL no alive worker takes: following its INVITE's cookie, placed
    /// with no memo of its INVITE, or met before the first view.
    Cancel,
    /// An out-of-dialog request: a retransmission following the cookie its
    /// first copy minted, or a new one met before the first view; or the ACK
    /// of a non-2xx final to an initial INVITE, which rides no cookie.
    Initial,
}

impl NoTargetDrop {
    pub const ALL: [NoTargetDrop; 3] =
        [NoTargetDrop::InDialog, NoTargetDrop::Cancel, NoTargetDrop::Initial];

    pub fn of(req: &SipRequest) -> Self {
        if req.method() == Method::Cancel {
            NoTargetDrop::Cancel
        } else if req.to().tag().is_some() {
            NoTargetDrop::InDialog
        } else {
            NoTargetDrop::Initial
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            NoTargetDrop::InDialog => "in_dialog",
            NoTargetDrop::Cancel => "cancel",
            NoTargetDrop::Initial => "initial",
        }
    }
}

impl ProxyMetrics {
    /// Count one request dropped for want of an alive target, under its
    /// `kind`; each retransmission counts.
    pub fn record_no_target_dropped(&self, kind: NoTargetDrop) {
        self.no_target_dropped.add(&[kind.as_str()], 1);
    }

    pub fn no_target_dropped_count(&self, kind: NoTargetDrop) -> u64 {
        self.no_target_dropped.get(&[kind.as_str()])
    }
}
