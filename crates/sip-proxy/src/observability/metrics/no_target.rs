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
    /// A CANCEL following its INVITE's cookie, or met before the first view.
    Cancel,
    /// An out-of-dialog request: a retransmission following the cookie its
    /// first copy minted, or a new one met before the first view.
    Initial,
}

impl NoTargetDrop {
    /// Every kind, in declaration order.
    pub const ALL: [NoTargetDrop; 3] =
        [NoTargetDrop::InDialog, NoTargetDrop::Cancel, NoTargetDrop::Initial];

    /// The kind of `req`: a CANCEL, a request whose To names a tag (inside
    /// a dialog), or another (out of dialog).
    pub fn of(req: &SipRequest) -> Self {
        if req.method() == Method::Cancel {
            NoTargetDrop::Cancel
        } else if req.to().tag().is_some_and(|t| !t.is_empty()) {
            NoTargetDrop::InDialog
        } else {
            NoTargetDrop::Initial
        }
    }

    /// The `kind` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            NoTargetDrop::InDialog => "in_dialog",
            NoTargetDrop::Cancel => "cancel",
            NoTargetDrop::Initial => "initial",
        }
    }
}

impl ProxyMetrics {
    /// Count one request dropped because neither worker its cookie names is
    /// alive, for `sip_proxy_no_target_dropped_total{kind}`; each
    /// retransmission counts.
    pub fn record_no_target_dropped(&self, kind: NoTargetDrop) {
        self.no_target_dropped.add(&[kind.as_str()], 1);
    }

    /// Requests of `kind` dropped for want of an alive target so far.
    pub fn no_target_dropped_count(&self, kind: NoTargetDrop) -> u64 {
        self.no_target_dropped.get(&[kind.as_str()])
    }
}
