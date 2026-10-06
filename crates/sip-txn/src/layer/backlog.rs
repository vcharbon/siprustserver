//! Admission against the deferred backlog (ADR-0037 item 6): a new INVITE
//! arriving while the backlog holds its class's ceiling is refused
//! statelessly, before the 100 Trying and before any transaction exists, so
//! the retry deque stays bounded while the consumer is not draining the
//! output queue. The refusal and its memo are the node's shared
//! [`InviteRefusals`]: every later copy of one INVITE draws the same refusal,
//! whichever stage refused it first, and its ACK ends here.

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;

use sip_message::SipRequest;
use sip_net::UdpEndpoint;

use super::owner::Owner;
use super::refusals::Admission;

/// Ceilings on the deferred backlog (the critical events a full output queue
/// holds for later), in events, and the classifier a new INVITE is judged
/// with. A CANCEL, an ACK, a response and every event the layer emits for a
/// transaction it already holds are never refused; a retransmission of an
/// admitted INVITE matches its transaction first and is never judged.
#[derive(Debug, Clone, Copy)]
pub struct DeferredBound {
    /// The ceiling for [`InviteClass::Normal`].
    pub normal: usize,
    /// The ceiling for [`InviteClass::Emergency`] and
    /// [`InviteClass::InDialog`]: this layer cannot tell a dialog the
    /// consumer holds from one it never had, and a 503 leaves an existing
    /// dialog in place (RFC 3261 §12.2.1.2, §14.1). Read as at least
    /// `normal`.
    pub emergency: usize,
    /// The class of an INVITE no transaction holds.
    pub class_of: fn(&SipRequest) -> InviteClass,
}

impl DeferredBound {
    /// The ceiling an INVITE of `class` is refused at.
    fn ceiling(&self, class: InviteClass) -> usize {
        match class {
            InviteClass::Normal => self.normal,
            InviteClass::Emergency | InviteClass::InDialog => self.emergency.max(self.normal),
        }
    }

    /// Whether an INVITE of `class` is refused while `deferred` events wait.
    pub fn refuses(&self, class: InviteClass, deferred: usize) -> bool {
        deferred >= self.ceiling(class)
    }
}

/// The class an INVITE is judged in: the `class` of the refusal counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InviteClass {
    /// A new initial INVITE.
    Normal,
    /// A new initial INVITE carrying an emergency priority (RFC 4412).
    Emergency,
    /// An INVITE carrying a To-tag.
    InDialog,
}

impl InviteClass {
    /// Every class, in exposition order.
    pub const ALL: [InviteClass; 3] = [Self::Normal, Self::Emergency, Self::InDialog];

    /// Stable metric label (`class="..."`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Emergency => "emergency",
            Self::InDialog => "in_dialog",
        }
    }

    /// The class's position in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Whether the new INVITE judged now is admitted: no transaction is opened
/// for one that is not.
pub(super) enum NewInvite {
    /// Answered with the node's refusal.
    Refused,
    /// Admitted; `held` when the shared refusals hold its identity for the
    /// transaction about to open.
    Admitted { held: bool },
}

impl Owner {
    /// Judge the INVITE `req`, which no transaction holds, against the node's
    /// refusals: a copy of a refused INVITE draws that refusal again, whatever
    /// the backlog holds now — the caller has its final, or will have it from
    /// this copy, and a call admitted behind it would ring for no one; past its
    /// class's ceiling it is refused now, counted once. A refusal goes to
    /// `src`, before any 100 Trying.
    pub(super) async fn judge_new_invite(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: &SipRequest,
        src: SocketAddr,
    ) -> NewInvite {
        let Some(refusals) = self.refusals.clone() else {
            return NewInvite::Admitted { held: false };
        };
        let class = self.backlog_refuses(req);
        match refusals.admit(req, class.is_some()) {
            Admission::Admit { held } => return NewInvite::Admitted { held },
            Admission::Repeat => {
                self.metrics.refused_copies.fetch_add(1, Relaxed);
            }
            Admission::Refuse => {
                if let Some(class) = class {
                    self.metrics.deferred_refused[class.index()].fetch_add(1, Relaxed);
                }
            }
        }
        let refusal = refusals.answer(req);
        self.send_buffer(endpoint, refusal.image(), src).await;
        NewInvite::Refused
    }

    /// The class `req` is refused under when the backlog holds that class's
    /// ceiling now; `None` while it has room.
    fn backlog_refuses(&self, req: &SipRequest) -> Option<InviteClass> {
        let bound = self.deferred_bound.as_ref()?;
        let backlog = self.deferred_events.len();
        // Below the normal ceiling no class is refused: no INVITE is
        // classified on the uncongested path.
        if backlog < bound.normal {
            return None;
        }
        let class = (bound.class_of)(req);
        bound.refuses(class, backlog).then_some(class)
    }

    /// Whether `ack`, matching no transaction, acknowledges a refusal of the
    /// last 64·T1: it ends here, as a transaction's ACK to its non-2xx final
    /// would (RFC 3261 §17.2.1).
    pub(super) fn acknowledges_refusal(&self, ack: &SipRequest) -> bool {
        self.refusals.as_ref().is_some_and(|r| r.remembers() && r.refused(ack))
    }

    /// Release the shared refusals' hold on the identity of a leaving
    /// transaction that [`judge_new_invite`](Self::judge_new_invite) admitted.
    pub(super) fn release_hold(&self, branch: &str, call_id: &str, from_tag: &str) {
        if let Some(refusals) = &self.refusals {
            refusals.release(branch, call_id, from_tag);
        }
    }
}
