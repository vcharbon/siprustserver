//! [`NewCallGate`]: a consumer's switch on new calls, read at receipt. While
//! the consumer holds it set, an INVITE of the normal class that no
//! transaction holds is refused before any 100 Trying or transaction exists,
//! with the node's shared refusal ([`InviteRefusals`](crate::InviteRefusals)),
//! as at the deferred backlog's ceiling: a copy draws the same refusal and is
//! not judged again. Emergency and in-dialog INVITEs pass. The layer only
//! reads the switch, one atomic load per INVITE it judges; the consumer is
//! its one writer, and the layer reads the last state it wrote.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

use sip_message::SipRequest;

use crate::invite_class::InviteClass;

/// The switch and the classifier a new INVITE is judged with.
#[derive(Clone)]
pub struct NewCallGate {
    refusing: Arc<AtomicBool>,
    class_of: fn(&SipRequest) -> InviteClass,
}

impl NewCallGate {
    /// A gate refusing new normal INVITEs while `refusing` reads `true`, each
    /// classified by `class_of`.
    pub fn new(refusing: Arc<AtomicBool>, class_of: fn(&SipRequest) -> InviteClass) -> Self {
        Self { refusing, class_of }
    }

    /// The class `req` is refused under now; `None` while the switch is off
    /// or for an emergency or in-dialog INVITE. Classifies only while on.
    pub(super) fn refuses(&self, req: &SipRequest) -> Option<InviteClass> {
        if !self.refusing.load(Relaxed) {
            return None;
        }
        let class = (self.class_of)(req);
        (class == InviteClass::Normal).then_some(class)
    }
}
