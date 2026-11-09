//! The context a rule reads: the event, the call through the narrow
//! [`RuleCall`] view, and what the engine resolved for the event.

use call::{Dialog, Direction, Leg, MachineId, Obligation, TimerKey, TimerType};
use sip_message::{Method, SipRequest, SipResponse};

use crate::config::B2buaConfig;
use crate::event::CallEvent;
use crate::glare::GlareRefusal;
use crate::lifecycle::LifecycleEvent;
#[cfg(doc)]
use crate::model::{Match, RuleAction};
use crate::rule_call::RuleCall;

/// The context a rule sees, built from the event and the call: the call through the
/// narrow [`RuleCall`] view, the full `Call` staying on the framework's side (ADR-0020 X8).
pub struct RuleContext<'a> {
    pub call: RuleCall<'a>,
    pub call_ref: &'a str,
    pub event: &'a CallEvent,
    pub source_leg_id: &'a str,
    pub direction: Direction,
    pub now_ms: i64,
    pub config: &'a B2buaConfig,
    /// The dialog-level ladder this event's ACK or PRACK discharged before the
    /// rules ran (ADR-0032 X4) — the engine already retired its timers and
    /// retained emission; a rule reads the fact and owns none of the plumbing.
    /// `None` for every other event, and for an ACK or PRACK that named no
    /// live ladder (a retransmitted ACK, a repeat PRACK).
    pub discharged: Option<&'a Obligation>,
    /// What the engine's dialog gate resolved the event's message to before
    /// the rules ran: the dialog it names on the source leg, and the relay a
    /// response answers. Empty for any other event.
    pub resolved: &'a call::helpers::Resolved,
}

impl<'a> RuleContext<'a> {
    pub fn request(&self) -> Option<&SipRequest> {
        match self.event {
            CallEvent::Sip { message, .. } => match message.as_ref() {
                sip_message::SipMessage::Request(r) => Some(r),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn response(&self) -> Option<&SipResponse> {
        match self.event {
            CallEvent::Sip { message, .. } => match message.as_ref() {
                sip_message::SipMessage::Response(r) => Some(r),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn timer_type(&self) -> Option<&TimerType> {
        self.timer_key().map(|key| &key.timer_type)
    }
    /// For a lifecycle delivery: the event delivered.
    pub fn lifecycle(&self) -> Option<&LifecycleEvent> {
        match self.event {
            CallEvent::Lifecycle(e) => Some(e),
            _ => None,
        }
    }
    /// For a timer fire: the key of the timer that fired.
    pub fn timer_key(&self) -> Option<&TimerKey> {
        match self.event {
            CallEvent::Timer { key, .. } => Some(key),
            _ => None,
        }
    }
    /// For a fired [`TimerType::Service`] timer: `(owning service, key)`. `None`
    /// for every other event (including core timer fires). The branch point for
    /// a rule that funnels several watchdog keys through one
    /// [`Match::service_timers`] matcher.
    pub fn service_timer_key(&self) -> Option<(&MachineId, &str)> {
        match self.timer_type() {
            Some(TimerType::Service { service_id, key }) => Some((service_id, key)),
            _ => None,
        }
    }
    /// For a `Cancelled` event: did the CANCEL match an **in-dialog** INVITE
    /// server transaction (a re-INVITE — its `To` carried a tag)? `false` for
    /// the initial INVITE and for every other event kind. RFC 3261 §9: this is
    /// the discriminator between "caller abandoned the call" and "caller
    /// abandoned one renegotiation".
    pub fn cancelled_in_dialog(&self) -> bool {
        matches!(self.event, CallEvent::Cancelled { in_dialog: true, .. })
    }
    /// For a `Cancelled` event: the CANCEL's own header lines, empty for every
    /// other event. The transaction layer answered the CANCEL, so this is the
    /// only place the canceller's `Reason` (RFC 3326 §2) is ever stated.
    pub fn cancelled_headers(&self) -> &'a [sip_message::SipHeader] {
        match self.event {
            CallEvent::Cancelled { headers, .. } => headers,
            _ => &[],
        }
    }
    /// For a `Cancelled` event: the CSeq number of the INVITE transaction the
    /// CANCEL matched (the canceller's own CSeq space — i.e. the relayed
    /// pending request's `inbound_cseq`).
    pub fn cancelled_invite_cseq(&self) -> Option<i64> {
        match self.event {
            CallEvent::Cancelled { invite_cseq, .. } => invite_cseq.map(i64::from),
            _ => None,
        }
    }
    /// Is the leg a [`RuleAction::RelayToPeer`] of the current request would
    /// target in a **relayable** state? `false` exactly when the
    /// relay would go nowhere useful: no peer leg resolves, the peer leg is
    /// `Terminated` (e.g. a failed b-leg whose `/call/failure` reroute is still
    /// pending), or no target dialog resolves (a replacement leg still
    /// `Trying`). An `Early` peer dialog counts as relayable (RFC 3311 §5.1 early-dialog UPDATE is the normal case). Uses
    /// the SAME resolver as the executor's relay path
    /// ([`call::helpers::resolve_relay_peer`]), so match and action never
    /// disagree. Exposed on the context (usable from [`Match::filter`]) so a
    /// service rule can own failover-pending policy; the CORE default is the
    /// `update-peer-unavailable` local 491.
    pub fn peer_relay_ready(&self) -> bool {
        call::helpers::relay_peer_dialog_ready(
            self.call.0,
            self.source_leg_id,
            self.request_dialog_id(),
        )
    }
    /// Whether a [`RuleAction::RelayToPeer`] of the current event reaches any
    /// leg, by the resolver the executor's relay path reads
    /// ([`call::helpers::resolve_relay_peer`]).
    pub fn relay_peer_resolves(&self) -> bool {
        matches!(
            call::helpers::resolve_relay_peer(
                self.call.0,
                self.source_leg_id,
                self.request_dialog_id()
            ),
            call::helpers::RelayPeer::Leg { .. }
        )
    }

    /// Whether the current INVITE meets glare (RFC 3261 §14.1), by the same
    /// predicate the exchange count reads ([`call::helpers::invite_glare`]):
    /// an INVITE transaction open on the source dialog or its relay's, or
    /// the sender's dialog-creating INVITE still without a final.
    pub fn invite_glare(&self) -> bool {
        call::helpers::invite_glare(self.call.0, self.source_leg_id, self.request_dialog_id())
    }

    /// The refusal the current UPDATE draws (RFC 3311 §5.2), if any: 500 over
    /// its sender's UPDATE still awaiting its final, whatever its body; with an
    /// offer, 500 over the sender's open INVITE offer and 491 over this stack's
    /// ([`crate::open_offer::open_offer`]).
    pub fn update_refusal(&self) -> Option<GlareRefusal> {
        let req = self.request().filter(|r| *r.method() == Method::Update)?;
        let source = self.request_dialog_id();
        if call::helpers::peer_update_pending(self.call.0, self.source_leg_id, source) {
            return Some(GlareRefusal::Unanswered);
        }
        req.sdp()?;
        match crate::open_offer::open_offer(self.call.0, self.source_leg_id, source)? {
            crate::open_offer::OpenOffer::Sender => Some(GlareRefusal::Unanswered),
            crate::open_offer::OpenOffer::Stack => Some(GlareRefusal::Crossing),
        }
    }

    /// The §14.2 / RFC 3311 §5.2 refusal the current request draws, if any:
    /// an INVITE meeting glare by [`Self::glare_refusal`], an UPDATE by
    /// [`Self::update_refusal`].
    pub fn offer_refusal(&self) -> Option<GlareRefusal> {
        if let Some(refusal) = self.update_refusal() {
            return Some(refusal);
        }
        let invite = self.request().is_some_and(|r| *r.method() == Method::Invite);
        (invite && self.invite_glare()).then(|| self.glare_refusal())
    }

    /// How the current INVITE's glare is refused (RFC 3261 §14.2): by what is
    /// open on the dialog, the sender's own earlier INVITE first.
    pub fn glare_refusal(&self) -> GlareRefusal {
        let source = self.request_dialog_id();
        if call::helpers::sender_invite_unanswered(self.call.0, self.source_leg_id, source) {
            GlareRefusal::Unanswered
        } else if call::helpers::sender_invite_unacknowledged(
            self.call.0,
            self.source_leg_id,
            source,
        ) {
            GlareRefusal::Unacknowledged
        } else {
            GlareRefusal::Crossing
        }
    }

    /// The leg the event arrived on.
    pub fn source_leg(&self) -> Option<&'a Leg> {
        if self.source_leg_id == self.call.a_leg().leg_id {
            Some(self.call.a_leg())
        } else {
            self.call.b_legs().iter().find(|l| l.leg_id == self.source_leg_id)
        }
    }
    /// The dialog of the source leg the current message names, as the
    /// engine's dialog gate resolved it ([`Self::resolved`]): a request's
    /// To-tag is ours and its From-tag the peer's, a response's the reverse
    /// (RFC 3261 §12.2). `None` for any other event, and when the message
    /// names no dialog of the leg.
    pub fn dialog(&self) -> Option<&'a Dialog> {
        let id = self.resolved.dialog.as_ref()?;
        self.source_leg()?.dialogs.by_tags(&id.our_tag, &id.peer_tag)
    }
    /// The dialog of the source leg the current request names, as the
    /// engine's dialog gate resolved it ([`Self::resolved`]); `None` for any
    /// other event and for a request naming no dialog.
    pub fn request_dialog_id(&self) -> Option<&'a call::DialogId> {
        self.request()?;
        self.resolved.dialog.as_ref()
    }
    /// The dialog a [`RuleAction::RelayToPeer`] of the current request would be
    /// regenerated on, resolved with the SAME resolver as the relay executor
    /// ([`call::helpers::relay_peer_dialog`]) so match and action never
    /// disagree. `None` when no peer leg or dialog resolves.
    pub fn peer_dialog(&self) -> Option<&'a Dialog> {
        call::helpers::relay_peer_dialog(self.call.0, self.source_leg_id, self.request_dialog_id())
            .map(|(_, d)| d)
    }
}
