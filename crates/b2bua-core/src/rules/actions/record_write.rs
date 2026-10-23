//! The actions that write the call's record or its timers, or hand a request
//! to a service or the limiter, with no SIP leaving: the record half of the
//! dispatch table ([`super::dispatch`]).

use call::helpers::{
    add_cdr_event, deactivate_rule, mark_decision, merge_leg, remove_relay, split_leg, Scope,
};
use call::{Call, CdrEvent};

use crate::effects::{CriticalStateEffect, HandlerEffects};
use b2bua_sdk::model::RuleAction;

use super::select::find_pending_dialog;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Apply `action`, one of the record writes [`Self::apply`] hands here;
    /// any other action is `apply`'s own.
    pub(super) fn write_record(
        &mut self,
        action: &RuleAction,
        call: &mut Call,
        fx: &mut HandlerEffects,
    ) {
        match action {
            RuleAction::Merge { leg_a, leg_b } => {
                *call = merge_leg(call.clone(), leg_a.clone(), leg_b.clone());
            }
            RuleAction::Split { leg_id } => {
                *call = split_leg(call.clone(), leg_id);
            }
            RuleAction::ResolveCancelledReinvite { leg_id, outbound_cseq } => {
                // Drop the cancelled pending-relay snapshot: the final response
                // to the CANCELled relayed re-INVITE resolves here, never
                // relayed (the txn layer already 487'd the originator).
                if let Some((t_id, _)) = find_pending_dialog(call, leg_id, *outbound_cseq) {
                    *call = remove_relay(call.clone(), leg_id, &t_id, *outbound_cseq);
                }
            }
            RuleAction::ScheduleTimer { key, delay } => {
                self.schedule(call, fx, key.clone(), delay.as_millis());
            }
            RuleAction::CancelTimer { key } => self.disarm(call, fx, key),
            RuleAction::CancelAllTimers => {
                call.timers.clear();
                fx.critical.push(CriticalStateEffect::CancelAllTimers);
            }
            RuleAction::AddCdrEvent { event_type, leg_id, status_code, reason } => {
                *call = add_cdr_event(
                    call.clone(),
                    CdrEvent {
                        event_type: *event_type,
                        timestamp: self.now_ms,
                        leg_id: leg_id.clone(),
                        status_code: *status_code,
                        reason: reason.clone(),
                        decision_ordinal: 0,
                    },
                );
            }
            RuleAction::MarkDecision { kind, leg_id, label } => {
                *call =
                    mark_decision(call.clone(), self.now_ms, *kind, leg_id.clone(), label.clone());
            }
            RuleAction::DeactivateRule { rule_id } => {
                *call = deactivate_rule(call.clone(), rule_id);
            }
            RuleAction::SetState { machine, to } => {
                // The sole writer of `sm_cursors` (ADR-0016 X4). Transition
                // legality is enforced by the executor against the winning
                // rule's declared edges.
                call.sm_cursors.insert(machine.clone(), to.clone());
            }
            RuleAction::ClearState { machine } => {
                // Machine deactivation (ADR-0016 X9): remove the cursor, returning
                // the machine to dormant — the declarative inverse of `SetState`,
                // realising the transition to the terminal `[*]`. Idempotent.
                // The machine's own timers retire with it.
                call.sm_cursors.remove(machine);
                self.retire(call, fx, Scope::Machine(machine));
            }
            RuleAction::SetPolicyUpdateBody { body } => {
                call.policy_update_body = Some(call::PolicyUpdateBody::Bytes(body.clone()));
            }
            RuleAction::SetPromotePem { state } => {
                *call = call::helpers::set_promote_pem(call.clone(), state.clone());
            }
            RuleAction::ReferAsyncHttp { request } => {
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::ReferAsyncHttp {
                    call_ref: call.call_ref.clone(),
                    request: request.clone(),
                });
            }
            RuleAction::ServiceHttpRequest {
                correlation_id,
                endpoint,
                method,
                headers,
                body,
                content_type,
                timeout_ms,
            } => {
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::ServiceHttpRequest {
                    call_ref: call.call_ref.clone(),
                    correlation_id: correlation_id.clone(),
                    endpoint: endpoint.clone(),
                    method: method.clone(),
                    headers: headers.clone(),
                    body: body.clone(),
                    content_type: content_type.clone(),
                    timeout_ms: *timeout_ms,
                });
            }
            RuleAction::SetTransfer { state } => {
                *call = call::helpers::set_transfer(call.clone(), state.clone());
            }
            RuleAction::FailureAsyncHttp { request } => {
                // One change number per admit the failover chain may send.
                let chain = u64::from(crate::answer_deadline::FAILURE_CHAIN);
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::FailureAsyncHttp {
                    call_ref: call.call_ref.clone(),
                    request: request.clone(),
                    limiter: call.limiter.owe_consult(chain),
                    deadline: None,
                });
            }
            RuleAction::ReleaseAsyncHttp { request } => {
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::ReleaseAsyncHttp {
                    call_ref: call.call_ref.clone(),
                    request: request.clone(),
                    limiter: call.limiter.owe_consult(1),
                    deadline: None,
                });
            }
            RuleAction::ReplaceAdmissionSet { correlation_id, entries, moves_call } => {
                let key = call.limiter.key().to_string();
                match call.limiter.replace_set(entries.clone(), *moves_call) {
                    call::Replacement::Send { change, held } => {
                        fx.fire_and_forget.push(
                            crate::effects::FireAndForgetEffect::LimiterAdmit {
                                call_ref: call.call_ref.clone(),
                                correlation_id: correlation_id.clone(),
                                key,
                                change,
                                held,
                                entries: entries.clone(),
                            },
                        );
                    }
                    call::Replacement::NothingToSend { change } => {
                        // The result re-enters as not sent.
                        let result = crate::limiter::report::LimiterAdmitResult {
                            call_ref: call.call_ref.clone(),
                            correlation_id: correlation_id.clone(),
                            report: call::AdmitReport {
                                key,
                                change,
                                entries: entries.clone(),
                                outcome: call::AdmitOutcome::NotSent,
                            },
                        };
                        fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::Reenter(
                            Box::new(result.into_event()),
                        ));
                    }
                }
            }
            RuleAction::SetSubscriptions { events } => {
                // The latest applied (re)route's declaration replaces the set
                // (decision-response parity with `SetFeatures`).
                call.subscriptions = events.clone();
            }
            RuleAction::SetReroute { state } => {
                *call = call::helpers::set_reroute(call.clone(), state.clone());
            }
            RuleAction::SetFeatures { features } => {
                // The call-lifetime latch: the standing withheld option tags
                // union into the incoming declaration (`apply_route` parity).
                let mut features = features.clone();
                features.latch_call_lifetime(call.features.as_ref());
                call.features = Some(features);
            }
            RuleAction::SetStatedHeaders { headers } => {
                if let Some(features) = call.features.as_mut() {
                    features.stated_headers = headers.clone();
                }
            }
            RuleAction::MergeCallExt { ext } => {
                for (service_id, value) in ext {
                    let v = (!value.is_null()).then(|| value.clone());
                    *call = call::helpers::set_call_ext(call.clone(), service_id, v);
                }
            }
            _ => {}
        }
    }
}
