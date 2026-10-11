//! [`RuleCall`]: the refined, read-only view of the call a rule reads.

use std::collections::BTreeMap;

use call::features::{FeatureActivations, RelayFirst18xStrategy};
use call::helpers::{PrackDisposition, RAckTokens};
use call::{
    ALegInviteSnapshot, ActivePeer, Call, CallModelState, CdrEvent, Dialog, ExtMap, Leg, MachineId,
    Obligation, PromotePemState, RSeq, StateLabel, TransferState,
};

/// The **refined view** of a [`Call`] a rule reads (ADR-0020 X8) — the entire
/// read surface a rule handler, `Match::filter` guard, or service `init` hook
/// gets. A borrowed newtype exposing only the *semantic* call state; there is
/// deliberately **no `Deref` and no raw escape hatch** to the underlying
/// `Call`, so framework internals are not part of the rule-author interface
/// and can change freely. The write side is unchanged: rules mutate only by
/// returning [`RuleAction`](crate::model::RuleAction)s through the executor.
///
/// Hidden by design (the framework-internal fields — do not add accessors for
/// these): `topology` (the HA `(p,b)` version vector), `worker_index`,
/// `sampled`/`trace_id`/`root_span_id` (observability), `message_count`,
/// `terminating_refresh_legs`, `a_leg_pending_vias`/`a_leg_pending_cseq`
/// (relay frame state), `limiter` (but its held and target sets), `timers`, `active_rules`,
/// `policy_update_headers`/`policy_update_body`, `billing_context`,
/// `emergency`. Add an accessor only when a real rule needs it — never
/// speculatively.
#[derive(Clone, Copy)]
pub struct RuleCall<'a>(pub(crate) &'a Call);

impl<'a> RuleCall<'a> {
    /// Wrap a call for rule consumption. Built by the engine at the dispatch
    /// boundary (router / seed path); constructing one grants only the narrow
    /// read surface below.
    pub fn new(call: &'a Call) -> Self {
        Self(call)
    }

    // ── identity / lifecycle ─────────────────────────────────────────────
    pub fn call_ref(&self) -> &'a str {
        &self.0.call_ref
    }
    pub fn state(&self) -> CallModelState {
        self.0.state
    }
    /// What a PRACK in this stack's `tag` dialog naming `rack` draws (RFC 3262
    /// §3): relayed, answered 200 here, or refused 481. `discharged` is the
    /// obligation the engine retired for the turn's own request.
    pub fn prack_disposition(
        &self,
        tag: &str,
        rack: RAckTokens,
        discharged: Option<&Obligation>,
    ) -> PrackDisposition {
        call::helpers::prack_disposition(self.0, tag, rack, discharged)
    }
    /// Whether `branch` is a PRACK client transaction this stack originated
    /// itself, never one it relayed.
    pub fn own_prack_branch(&self, branch: &str) -> bool {
        call::helpers::own_prack_branch(self.0, branch)
    }
    /// The relayed INVITE still pending toward its target — `(leg_id,
    /// outbound_cseq)` there — that the provisional shown as `rseq` in this
    /// stack's `tag` dialog answers; `None` for the initial INVITE or a
    /// transaction already resolved.
    pub fn pending_invite_answered_by(&self, tag: &str, rseq: RSeq) -> Option<(String, i64)> {
        call::helpers::pending_invite_answered_by(self.0, tag, rseq)
    }
    /// The leg whose dialog carries `shown_tag` as this stack's own — the face
    /// a reliable provisional so tagged was shown on.
    pub fn leg_shown(&self, shown_tag: &str) -> Option<&'a str> {
        call::helpers::leg_shown(self.0, shown_tag)
    }
    /// Whether `obligation` is the caller's ACK of the a-leg's INITIAL answer
    /// rather than of a re-INVITE's 2xx — what tells the `RepeatGiveUp` of the
    /// call's own setup from an in-dialog one (RFC 3261 §13.3.1.4). A rule
    /// answering either give-up may re-author the teardown — its cause, its
    /// CDR, the order the legs go — but not decline it: a call left Active
    /// after an `AckOf2xx` give-up is torn down by the framework with the CORE
    /// verdict (ADR-0032 X5). An `AwaitPrack` give-up carries no such floor.
    pub fn answers_initial_invite(&self, obligation: &Obligation) -> bool {
        call::helpers::answers_initial_invite(self.0, obligation)
    }
    /// Every machine's current cursor (ADR-0016 X4) — read-only; `SetState` /
    /// `ClearState` are the only writers.
    pub fn sm_cursors(&self) -> &'a BTreeMap<MachineId, StateLabel> {
        &self.0.sm_cursors
    }
    pub fn created_at(&self) -> i64 {
        self.0.created_at
    }

    // ── legs ─────────────────────────────────────────────────────────────
    pub fn a_leg(&self) -> &'a Leg {
        &self.0.a_leg
    }
    pub fn b_legs(&self) -> &'a [Leg] {
        &self.0.b_legs
    }
    /// The id the next leg this stack dials takes — the one a `CreateLeg`
    /// ([`crate::model::RuleAction::CreateLeg`]) emitted now mints.
    pub fn next_b_leg_id(&self) -> String {
        call::helpers::next_b_leg_id(self.0)
    }
    /// The routing answers the call dialled, in opening order.
    pub fn attempts(&self) -> &'a [call::Attempt] {
        &self.0.attempts
    }
    /// The id the next attempt opens under — the one a `CreateLeg` emitted
    /// now opens ([`crate::model::RuleAction::CreateLeg`]).
    pub fn next_attempt_id(&self) -> call::AttemptId {
        call::helpers::next_attempt_id(self.0)
    }
    /// The attempt `id` names.
    pub fn attempt(&self, id: call::AttemptId) -> Option<&'a call::Attempt> {
        call::helpers::find_attempt(self.0, id)
    }
    /// The read-only snapshot of the original a-leg INVITE (URI/headers/body).
    pub fn a_leg_invite(&self) -> &'a ALegInviteSnapshot {
        &self.0.a_leg_invite
    }

    // ── routing / decision ───────────────────────────────────────────────
    /// The opaque decision-layer token (carries the reroute plan, ADR-0017).
    pub fn callback_context(&self) -> Option<&'a str> {
        self.0.callback_context.as_deref()
    }
    pub fn features(&self) -> Option<&'a FeatureActivations> {
        self.0.features.as_ref()
    }
    pub fn active_peer(&self) -> Option<&'a ActivePeer> {
        self.0.active_peer.as_ref()
    }
    /// The caller-facing dialog fronting `b_leg_id`'s dialog with peer tag
    /// `b_tag` ([`call::Dialog::fronts`]).
    pub fn fronting_dialog(&self, b_leg_id: &str, b_tag: &str) -> Option<&'a Dialog> {
        call::helpers::fronting_dialog(self.0, b_leg_id, b_tag)
    }
    /// Both peered leg ids (`active_peer`), or empty when split.
    pub fn all_peered_legs(&self) -> Vec<String> {
        call::helpers::all_peered_legs(self.0)
    }

    // ── admission ──────────────────────────────────────────────────────────
    /// The set the call limiter last stated it holds for the call: what a
    /// service computes its next replacement from
    /// ([`crate::model::RuleAction::ReplaceAdmissionSet`]), never from an admit
    /// whose answer it has not seen.
    pub fn limiter_held(&self) -> &'a [call::LimiterEntry] {
        self.0.limiter.held()
    }
    /// The set the call's latest admit asked for, from the turn that sends
    /// it (a replacement's) or from its report (a consult's); after a refused
    /// or superseded change, the held set. With
    /// [`limiter_held`](Self::limiter_held), what a service tells a confirmed
    /// hold from an unconfirmed add.
    pub fn limiter_target(&self) -> &'a [call::LimiterEntry] {
        self.0.limiter.target()
    }

    // ── service slices (typed data backing; the cursor lives in sm_cursors) ──
    pub fn ext(&self) -> Option<&'a ExtMap> {
        self.0.ext.as_ref()
    }
    pub fn transfer_state(&self) -> Option<&'a TransferState> {
        self.0.transfer.as_ref()
    }
    pub fn transfer_active(&self) -> bool {
        call::helpers::transfer_active(self.0)
    }
    /// The routing decision directed LOCAL REFER processing (`features.refer`)
    /// — the gate on the transfer seed rules.
    pub fn refer_processed_locally(&self) -> bool {
        call::helpers::refer_processed_locally(self.0)
    }
    pub fn relay_first_18x_strategy(&self) -> Option<RelayFirst18xStrategy> {
        call::helpers::relay_first_18x_strategy(self.0)
    }
    pub fn relay_first_18x_first_relayed(&self) -> bool {
        call::helpers::relay_first_18x_first_relayed(self.0)
    }
    pub fn relay_first_18x_stored_a_tag(&self) -> Option<&'a str> {
        call::helpers::relay_first_18x_stored_a_tag(self.0)
    }
    /// The active `Relay18xMessages` policy (defaults to `FIRST`).
    pub fn relay_first_18x_messages(&self) -> call::features::Relay18xMessages {
        call::helpers::relay_first_18x_messages(self.0)
    }
    /// Whether an 18x with this *upstream* status value was already relayed
    /// (the `ONE_PER_VALUE` dedupe test).
    pub fn relay_first_18x_value_relayed(&self, status: u16) -> bool {
        call::helpers::relay_first_18x_value_relayed(self.0, status)
    }
    pub fn promote_pem_state(&self) -> Option<&'a PromotePemState> {
        self.0.promote_pem.as_ref()
    }
    pub fn promote_pem_promoted(&self) -> bool {
        call::helpers::promote_pem_promoted(self.0)
    }
    pub fn promote_pem_window_open(&self) -> bool {
        call::helpers::promote_pem_window_open(self.0)
    }
    /// The SDP cached on a b-leg early dialog (`fake-prack`).
    pub fn cached_sdp_for_leg_dialog(&self, leg_id: &str, b_tag: &str) -> Option<&'a [u8]> {
        call::helpers::cached_sdp_for_leg_dialog(self.0, leg_id, b_tag)
    }

    // ── subscribed release events + established-call reroute ──────────────────
    /// The release events the decision backend subscribed to on the last
    /// applied `Route` (written via `SetSubscriptions` / `apply_route`).
    pub fn subscriptions(&self) -> &'a [call::ReleaseEventKind] {
        &self.0.subscriptions
    }
    /// Whether the backend subscribed to `event` — the gate that decides
    /// consult-the-engine vs act-locally when the event fires.
    pub fn subscribed(&self, event: call::ReleaseEventKind) -> bool {
        self.0.subscriptions.contains(&event)
    }
    /// The in-flight established-call reroute slice (written via `SetReroute`).
    pub fn reroute_state(&self) -> Option<&'a call::RerouteState> {
        self.0.reroute.as_ref()
    }
    /// Whether an established-call reroute is in flight (the activation guard
    /// for the `release-reroute` rules, mirroring `transfer_active`).
    pub fn reroute_active(&self) -> bool {
        self.0.reroute.is_some()
    }

    // ── bookkeeping (read-only; written via `AddCdrEvent`) ───────────────
    pub fn cdr_events(&self) -> &'a [CdrEvent] {
        &self.0.cdr_events
    }
    /// The count of decisions applied to the call so far — the ordinal every
    /// message and event written now is stamped with; `0` before the first
    /// decision. A service records its own marks under the same ordinal.
    pub fn decision_ordinal(&self) -> u32 {
        self.0.decision_ordinal
    }
}
