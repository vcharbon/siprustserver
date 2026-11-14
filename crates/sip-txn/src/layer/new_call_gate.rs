//! [`NewCallGate`]: a consumer's switch on new calls, and the age of its queue,
//! read at receipt. While the consumer holds the switch set, or while the
//! oldest event it left in the queue has waited the gate's age, an INVITE of
//! the normal class that no transaction holds is refused before any 100 Trying
//! or transaction exists, with the node's shared refusal
//! ([`InviteRefusals`](crate::InviteRefusals)), as at the deferred backlog's
//! ceiling: a copy draws the same refusal and is not judged again. Emergency
//! and in-dialog INVITEs pass. The layer only reads: one atomic load per INVITE
//! it judges, then the queue's age (a clock read and a few atomic loads). The
//! consumer is the switch's one writer, and a consumer held in one piece of
//! work writes nothing: the age is what still moves.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use sip_message::SipRequest;

use super::event_age::EventAges;
use crate::invite_class::InviteClass;

/// The switch, the queue's age and the classifier a new INVITE is judged with.
#[derive(Clone)]
pub struct NewCallGate {
    refusing: Arc<AtomicBool>,
    refusing_age: Option<Duration>,
    class_of: fn(&SipRequest) -> InviteClass,
}

impl NewCallGate {
    /// A gate refusing new normal INVITEs while `refusing` reads `true`, each
    /// classified by `class_of`.
    pub fn new(refusing: Arc<AtomicBool>, class_of: fn(&SipRequest) -> InviteClass) -> Self {
        Self { refusing, refusing_age: None, class_of }
    }

    /// This gate refusing also while the oldest event in the queue has waited
    /// `age` or longer.
    pub fn refusing_at_age(mut self, age: Duration) -> Self {
        self.refusing_age = Some(age);
        self
    }

    /// The class `req` is refused under now, `ages` the queue's stamps; `None`
    /// while the switch is off and the queue younger than the gate's age, or
    /// for an emergency or in-dialog INVITE. Classifies only while refusing.
    pub(super) fn refuses(&self, req: &SipRequest, ages: &EventAges) -> Option<InviteClass> {
        let aged = || self.refusing_age.is_some_and(|age| ages.oldest() >= age);
        if !self.refusing.load(Relaxed) && !aged() {
            return None;
        }
        let class = (self.class_of)(req);
        (class == InviteClass::Normal).then_some(class)
    }
}
