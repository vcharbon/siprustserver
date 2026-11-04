//! Which recorded events the RFC audit is handed: the one view of the wire
//! every rule judges, and the loss evidence the cross-message pass also reads.

use crate::contracts::SignalingNetworkEvent;
use crate::types::RecvDisposition;

/// Whether the RFC audit sees this event. `RecvConsumed` is a projection-only
/// consumption marker; a `RecvItem` is included per its disposition's
/// [`RecvDisposition::audit_visible`] (modeled loss / infra-absorbed duplicates
/// are deliberately invisible to the rules). EVERY rule invocation — the
/// per-bind Drop pass, the layer-close cross pass, and any external harness
/// running `rfc_*_rules` over a channel snapshot — judges only these, so the
/// rules keep one view of the wire; the cross pass is handed
/// [`cross_audit_event`]s and filters them itself.
pub fn audit_visible_event(event: &SignalingNetworkEvent) -> bool {
    match event {
        SignalingNetworkEvent::RecvConsumed { .. } => false,
        // A re-emit is a byte-identical retransmission — projection-only, so the
        // rules keep one un-duplicated view of the wire.
        SignalingNetworkEvent::ReEmit { .. } => false,
        SignalingNetworkEvent::RecvItem { disposition, .. } => disposition.audit_visible(),
        _ => true,
    }
}

/// Whether the cross-message pass is handed this event: an
/// [`audit_visible_event`], or an arrival the loss model dropped, which a rule
/// never judges and which says only that its sender sent it.
pub fn cross_audit_event(event: &SignalingNetworkEvent) -> bool {
    audit_visible_event(event)
        || matches!(
            event,
            SignalingNetworkEvent::RecvItem { disposition: RecvDisposition::LossModel, .. }
        )
}
