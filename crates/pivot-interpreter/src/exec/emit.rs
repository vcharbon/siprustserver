//! One send step put on the wire, or ruled moot.

use pivot_schema::bundle::{Dir, Failure, LadderSide};
use sip_message::{Method, SipMessage};

use super::completion::outcome_of;
use super::ladders::drawn_key;
use super::Runner;
use crate::close;
use crate::deviation::StepEffects;
use crate::gate::Inbound;
use crate::plan::{CompiledStep, Discriminator};
use crate::retransmit::{self, DrawnAck};
use crate::scripted_final;

impl<'a, 'p> Runner<'a, 'p> {
    /// Whether `step` is a BYE its leg's dialog no longer holds:
    /// [`close::bye_moot`] over the leg's recorded ladder. A BYE the document
    /// scripts behind an expect of the peer's BYE on its leg is the captured
    /// party's own act, 481 included, and is sent as written.
    pub(super) fn bye_moot(&mut self, step: &CompiledStep) -> bool {
        let is_bye = |s: &CompiledStep| {
            matches!(&s.discriminator, Discriminator::Request { method }
                if Method::from_wire(method) == Method::Bye)
        };
        let steps = self.instance.plan().steps();
        let scripted_behind = steps
            .iter()
            .take_while(|s| s.id != step.id)
            .any(|s| s.leg == step.leg && s.is_expect() && is_bye(s));
        is_bye(step)
            && !scripted_behind
            && close::bye_moot(
                &self.instance.recording().legs().remove(&step.leg).unwrap_or_default(),
            )
    }

    /// Whether `step` is a final to an INVITE its leg already answered: the
    /// INVITE it answers ([`scripted_final::answered_invite`]) arrived and
    /// already drew a final. One whose INVITE has not arrived is not moot.
    pub(super) fn final_moot(&self, step: &CompiledStep) -> bool {
        if !scripted_final::is_final_to_invite(step) {
            return false;
        }
        let ladder = self.instance.recording().legs().remove(&step.leg).unwrap_or_default();
        let steps = self.instance.plan().steps();
        let Some(at) = steps.iter().position(|s| s.id == step.id) else { return false };
        scripted_final::answered_invite(&steps, at, &ladder)
            .is_some_and(|cseq| scripted_final::invite_answered(&ladder, cseq))
    }

    /// Emit one send step.
    pub(super) async fn emit(
        &mut self,
        step: &CompiledStep,
        answering: Option<(u32, Option<String>)>,
    ) -> bool {
        let effects = StepEffects::of(self.instance.plan().deviations_for(&step.id));
        let refusals = effects.refusals();
        if !refusals.is_empty() {
            for failure in refusals {
                self.instance.fail(failure);
            }
            return false;
        }
        // A withheld automatic is not sent, and the flow moves past it: the
        // deviation IS the absence.
        if effects.suppressed {
            self.instance.recording().push(
                &step.leg,
                Dir::Out,
                self.now_us(),
                Vec::new(),
                Some(&step.id),
                Some("withheld by a suppress-auto deviation"),
            );
            self.complete(step);
            return true;
        }
        // RFC 3261 §15.1.2: a BYE on a dialog this leg already ended by
        // answering the peer's BYE names nothing and would draw a 481. It is
        // moot: never put on the wire, and the final it would have drawn is
        // retired with it.
        if self.bye_moot(step) {
            self.instance.recording().push(
                &step.leg,
                Dir::Out,
                self.now_us(),
                Vec::new(),
                Some(&step.id),
                Some("moot: the dialog ended when this leg answered the peer's BYE"),
            );
            self.complete(step);
            self.retire_answers_to(step);
            return true;
        }
        // RFC 3261 §17.2.1: a server transaction sends one final. A scripted
        // final to an INVITE this leg already answered is moot: never sent, so
        // its ladder never starts.
        if self.final_moot(step) {
            self.instance.recording().push(
                &step.leg,
                Dir::Out,
                self.now_us(),
                Vec::new(),
                Some(&step.id),
                Some("moot: the INVITE this final answers already has its final"),
            );
            self.complete(step);
            return true;
        }
        let answering = answering.as_ref().map(|(cseq, tag)| (*cseq, tag.as_deref()));
        let message = match self.compose(step, &effects, answering) {
            Ok(message) => message,
            Err(failure) => {
                self.instance.fail(*failure);
                return false;
            }
        };
        let wire = sip_message::serialize(&message);
        // An auto ACK's count is DRAWN by the wire, not paced (§6.3): one copy
        // per repeat of the final its transaction drew.
        let draws = drawn_key(step, &message);
        // A ladder this peer cannot pace refuses BEFORE the wire: emitting the
        // first copy and then discovering the repeats have no interval would
        // leave a half-reproduced ladder on the wire (§14).
        let ladder = match step.retransmits.filter(|n| *n > 0) {
            Some(_) if draws.is_some() => None,
            None => None,
            Some(count) => match retransmit::schedule_of(&wire, &step.retransmit_intervals_ms) {
                Ok(schedule) => Some((schedule, count)),
                Err(detail) => {
                    self.instance.fail(Failure::RetransmitLadderRefused {
                        step: step.id.clone(),
                        leg: step.leg.clone(),
                        detail,
                    });
                    return false;
                }
            },
        };
        let Some(actor) = self.instance.plan().actor_of_leg(&step.leg).map(str::to_string) else {
            self.instance.fail(Failure::SendFailed {
                step: step.id.clone(),
                leg: step.leg.clone(),
                detail: "the leg names no actor".into(),
            });
            return false;
        };
        let Some(agent) = self.lane.agents.get(&actor).cloned() else {
            self.instance.fail(Failure::SendFailed {
                step: step.id.clone(),
                leg: step.leg.clone(),
                detail: format!("the lane bound no agent for actor {actor:?}"),
            });
            return false;
        };
        // A response goes back where the request came from (§18.2.2); a request
        // goes to the lane's route target.
        let mut fallback: Option<String> = None;
        let dst = match self.stacks.get(&step.leg) {
            None => self.lane.route_target,
            Some(stack) if step.msg.status.is_some() => {
                match stack.response_target(step.msg.cseq_method.as_deref()) {
                    Some(addr) => addr,
                    None => {
                        fallback = Some(
                            "the answered request named no reachable Via sent-by; \
                             addressed at the lane's route target"
                                .to_string(),
                        );
                        stack.next_hop()
                    }
                }
            }
            Some(stack) => stack.next_hop(),
        };
        if let Err(e) = agent.try_send_datagram(&wire, dst).await {
            self.instance.fail(Failure::SendFailed {
                step: step.id.clone(),
                leg: step.leg.clone(),
                detail: e.to_string(),
            });
            return false;
        }
        self.instance.recording().push(
            &step.leg,
            Dir::Out,
            self.now_us(),
            wire.clone(),
            Some(&step.id),
            fallback.as_deref(),
        );
        // §15.1.2 owes this BYE a final response, and it arrives whether or not
        // the capture's vantage lasted long enough for the document to script
        // an expect for it.
        if let SipMessage::Request(request) = &message {
            if *request.method() == Method::Bye {
                self.byes_sent.insert((step.leg.clone(), request.cseq().seq()));
            }
        }
        // An expect-side ladder on this leg ends here where this is its answer:
        // the SUT stops on it, so a rung emitted just before it was a race.
        self.repeats.answered(&step.leg, &wire, self.now_us());
        self.repeats.claim(
            &step.id,
            &step.leg,
            LadderSide::Send,
            step.retransmits,
            &step.retransmit_intervals_ms,
            &wire,
            self.now_us(),
        );
        if let Some((schedule, count)) = ladder {
            self.schedule_ladder(step, &agent, &wire, dst, schedule, count);
        }
        if let Some(key) = draws {
            let ack =
                DrawnAck { step: step.id.clone(), leg: step.leg.clone(), wire: wire.clone(), dst };
            // Every copy of the final that arrived while this ACK was HELD is
            // still owed one (RFC 3261 §13.2.2.4), so the hold releases them all.
            let owed = self.drawn.sent(key, ack.clone());
            for _ in 0..owed {
                if !self.draw_ack(&ack).await {
                    return false;
                }
            }
        }
        self.instance.record_step(&step.id, outcome_of(&Inbound::of(&message)));
        self.sync_leg_state(&step.leg);
        self.complete(step);
        true
    }
}
