//! The new leg's INVITE (`CreateLeg`): behind the destination allow-list,
//! minted under the engine's next leg id, opening the attempt it dials.

use std::ops::ControlFlow;

use call::helpers::{add_cdr_event, add_originated_b_leg};
use call::{Call, CdrEvent, LegKind, TerminationCause};
use sip_message::header::HeaderName;

use crate::effects::HandlerEffects;
use crate::rules::capabilities;
use crate::rules::relay;
use b2bua_sdk::model::{AttemptOpening, Body, RuleContext};

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Build and send a new b-leg INVITE toward `destination`
    /// ([`b2bua_sdk::model::RuleAction::CreateLeg`]). Admission gate — same
    /// policy as `apply_route`: a rule-driven destination that doesn't pass the
    /// suffix allow-list is a config bug, refused so the call doesn't hang
    /// waiting for an answer that will never come. A refused leg is not built:
    /// a `Reject` CDR records the cause, the call ends by
    /// [`Self::begin_termination`] (every confirmed leg BYEd, every ringing one
    /// CANCELled) and `Break` stops the rule's remaining actions, which
    /// presumed the leg. An admitted leg is recorded with its `InviteSent` as
    /// its INVITE leaves, under the decision current at this turn. A leg
    /// dialled for an attempt opens it and joins it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create_leg(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        destination: &(String, u16),
        new_ruri: Option<&str>,
        new_from: Option<&str>,
        new_to: Option<&str>,
        callback_context: Option<&str>,
        body_override: Option<&Body>,
        header_updates: &[(String, Option<String>)],
        header_adds: &[(String, Vec<String>)],
        kind: Option<LegKind>,
        attempt: Option<&AttemptOpening>,
    ) -> ControlFlow<()> {
        if crate::destination_allowlist::classify_admission(
            &destination.0,
            &self.config.worker_allowed_target_suffixes,
        ) == crate::destination_allowlist::AdmissionVerdict::Reject
        {
            *call = add_cdr_event(
                call.clone(),
                CdrEvent {
                    event_type: call::CdrEventType::Reject,
                    timestamp: self.now_ms,
                    leg_id: ctx.source_leg_id.to_string(),
                    status_code: Some(503),
                    reason: Some(format!("admission_reject host={}", destination.0)),
                    decision_ordinal: 0,
                },
            );
            return self.refuse_leg(call, fx, ctx);
        }
        let leg_id = call::helpers::next_b_leg_id(call);
        let a_invite = self.caller_invite;
        let offers_sdp = relay::mints_offer(a_invite, body_override);
        // The originator's `Accept` states what she takes; a media leg answers
        // the service that dialled it, so that line stays behind unless an
        // update states one.
        let mut header_updates = header_updates.to_vec();
        if kind == Some(LegKind::Media)
            && !header_updates.iter().any(|(n, _)| HeaderName::Accept.matches(n))
        {
            header_updates.push((HeaderName::Accept.as_wire_str().to_string(), None));
        }
        let header_updates = header_updates.as_slice();
        let advertised = capabilities::relaying_for_leg(call, &leg_id, a_invite.headers());
        // Same refusal as the admission reject above, for the other way a
        // decision can name no destination: an address field that does not read.
        // The leg is not created and no INVITE goes out — originating on
        // a fabricated target would dial an address the decision never stated.
        let (mut leg, mut effect) = match relay::build_b_leg(
            relay::CallMarks::of(call),
            &leg_id,
            a_invite,
            destination.clone(),
            new_ruri,
            new_from,
            new_to,
            self.config,
            &mut self.turn.ids,
            body_override,
            header_updates,
            &advertised,
            crate::rules::charging::minting_arm(call, &leg_id, kind),
            &capabilities::withheld_option_tags(call, kind, offers_sdp),
            &capabilities::offered_option_tags(call, kind),
            kind,
            call.a_leg.invite_final_sent.is_none(),
            self.now_ms,
        ) {
            Ok(built) => built,
            Err(err) => {
                tracing::warn!(
                    call_ref = %call.call_ref,
                    %leg_id,
                    detail = %err.detail(),
                    "leg not created"
                );
                *call = add_cdr_event(
                    call.clone(),
                    CdrEvent {
                        event_type: call::CdrEventType::Reject,
                        timestamp: self.now_ms,
                        leg_id: ctx.source_leg_id.to_string(),
                        status_code: Some(500),
                        reason: Some(format!("unreadable_address field={}", err.field)),
                        decision_ordinal: 0,
                    },
                );
                return self.refuse_leg(call, fx, ctx);
            }
        };
        crate::rules::charging::uncharge_media_leg(
            call,
            kind,
            header_updates,
            &mut leg,
            &mut effect,
        );
        crate::rules::stated_headers::add_to_minted(&mut leg, &mut effect, header_adds);
        let author = match body_override {
            Some(body) => relay::Author::from(&body.author),
            None => relay::Author::Leg(&call.a_leg.leg_id),
        };
        leg.sdp_session = relay::opened(&effect, author);
        if let Some(ctx_str) = callback_context {
            call.callback_context = Some(ctx_str.to_string());
        }
        *call = add_originated_b_leg(call.clone(), leg, self.now_ms);
        fx.outbound.push(effect);
        // The leg rings for its attempt's deadline from its first 18x
        // (`ring_deadline`); a leg that opens no attempt rings under none.
        if let Some(opening) = attempt {
            let id = crate::attempts::open(call, fx, opening.clone());
            call::helpers::join_attempt(call, id, &leg_id);
        }
        ControlFlow::Continue(())
    }

    /// End the call whose rule asked for a leg that is not created.
    fn refuse_leg(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
    ) -> ControlFlow<()> {
        self.begin_termination(call, fx, ctx, None, (TerminationCause::Admission, None));
        ControlFlow::Break(())
    }
}
