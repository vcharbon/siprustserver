//! A script that cannot go on: where it ended, and the generic close's turns.

use std::collections::BTreeMap;

use pivot_schema::bundle::{Abandoned, CloseAct, Dir, Failure};
use sip_message::{Method, SipMessage};

use super::Runner;
use crate::close::{self, Owed};
use crate::stack::reason_for;

impl<'a, 'p> Runner<'a, 'p> {
    /// A run that CANNOT GO ON ends its SCRIPT (§11.2), whatever its polarity.
    ///
    /// The failure is untouched: raised with its site and its evidence, the
    /// datagram recorded verbatim, whatever this does. What it marks is only
    /// that the run stops FOLLOWING the flow — [`close`](crate::close) then ends
    /// what the scripted legs still hold and the run settles, instead of walking
    /// a tail whose next message it can no longer compose. A failure the run CAN
    /// go on past never reaches here: it is recorded and the flow continues
    /// ([`progress`](crate::progress)).
    pub(super) fn end_script(&mut self, leg: Option<&str>, step: Option<&str>) {
        if self.abandoned.is_some() {
            return;
        }
        self.abandoned = Some(Abandoned {
            leg: leg.map(str::to_string),
            step: step.map(str::to_string),
            pending: Vec::new(),
            closed: Vec::new(),
        });
    }

    /// State the abandonment in the verdict: where the script stopped, what it
    /// never ran, and what the close emitted in its place (§11.2).
    pub(super) fn state_abandonment(&mut self) {
        let Some(mut abandoned) = self.abandoned.take() else { return };
        abandoned.pending = self.instance.cursor().pending();
        self.instance.note_abandoned(abandoned);
    }

    /// What every scripted leg still holds open (§10, §11.2) — the close's own
    /// half of the settle condition, and the work of its next turn. Empty on a
    /// run whose script was not abandoned, and on one whose dialogs are all
    /// terminal.
    pub(super) fn close_obligations(&self) -> BTreeMap<String, Owed> {
        if self.abandoned.is_none() {
            return BTreeMap::new();
        }
        close::obligations(&self.instance.recording())
            .into_iter()
            .filter(|(leg, owed)| owed.open() && self.agent_of_leg(leg).is_some())
            .collect()
    }

    /// One turn of the generic close (§11.2): every scripted leg that owes an
    /// emission puts exactly one on the wire.
    ///
    /// One per leg per turn deliberately — the obligation is re-read off the
    /// recording each turn, so the ACK a leg owes and the BYE behind it are two
    /// turns, and whatever the platform sends in between is seen before the
    /// second one goes out.
    pub(super) async fn close_turn(&mut self, owed: BTreeMap<String, Owed>) {
        for (leg, owed) in owed {
            let Some(act) = owed.act() else { continue };
            if self.close_refused.contains(&leg) {
                continue;
            }
            let Some(agent) = self.agent_of_leg(&leg) else { continue };
            let (wire, dst) = match self.compose_close(&leg, &owed, "(close)") {
                Ok(composed) => composed,
                Err(failure) => {
                    self.close_refused.insert(leg);
                    self.instance.fail(*failure);
                    continue;
                }
            };
            if let Err(e) = agent.try_send_datagram(&wire, dst).await {
                self.close_refused.insert(leg.clone());
                self.instance.fail(Failure::SendFailed {
                    step: "(close)".into(),
                    leg: leg.clone(),
                    detail: format!("the generic close could not send: {e}"),
                });
                continue;
            }
            let sent = sip_message::sniff::first_line(&wire);
            // An emission the run authored ends a ladder as a scripted one does.
            self.repeats.answered(&leg, &wire, self.now_us());
            self.instance.recording().push(
                &leg,
                Dir::Out,
                self.now_us(),
                wire,
                None,
                Some(&format!("the generic close: this leg {owed}")),
            );
            if let Some(abandoned) = &mut self.abandoned {
                abandoned.closed.push(CloseAct { leg, owed: act, sent });
            }
        }
    }

    /// The message one obligation puts on the wire, and where it goes. `site`
    /// names what a refusal is attributed to, since no step owns the emission.
    ///
    /// Composed by the leg's OWN stack, so what is emitted is ordinary compliant
    /// SIP: the same dialog identity, the same CSeq space and the same Via the
    /// flow's own emissions carry.
    pub(super) fn compose_close(
        &mut self,
        leg: &str,
        owed: &Owed,
        site: &str,
    ) -> Result<(Vec<u8>, std::net::SocketAddr), Box<Failure>> {
        let fail = |detail: String| Failure::SendFailed {
            step: site.to_string(),
            leg: leg.to_string(),
            detail,
        };
        let route_target = self.lane.route_target;
        let stack = self.stacks.get_mut(leg).ok_or_else(|| fail("the leg has no stack".into()))?;
        let (message, dst) = match owed {
            Owed::Answer { cseq_method, cseq, to_tag, status, early_tag } => {
                let answer = crate::stack::Answer {
                    status: *status,
                    reason: reason_for(*status),
                    cseq_method: Some(cseq_method),
                    early_tag: early_tag.as_deref(),
                };
                let response = stack
                    .respond_to(*cseq, to_tag.as_deref(), &answer, &[], Vec::new(), None)
                    .map_err(|e| fail(e.to_string()))?;
                let dst = stack.response_target(Some(cseq_method)).unwrap_or(route_target);
                (SipMessage::Response(response), dst)
            }
            Owed::Ack(response) => (
                SipMessage::Request(
                    stack
                        .ack_for(response, &[], Vec::new(), None, None)
                        .map_err(|e| fail(e.to_string()))?,
                ),
                stack.next_hop(),
            ),
            Owed::Bye => (
                SipMessage::Request(
                    stack
                        .in_dialog(Method::Bye, &[], Vec::new(), None, None)
                        .map_err(|e| fail(e.to_string()))?,
                ),
                stack.next_hop(),
            ),
            Owed::Cancel => (
                SipMessage::Request(stack.cancel(&[]).map_err(|e| fail(e.to_string()))?),
                stack.next_hop(),
            ),
            // Nothing to compose: the caller filtered these out by asking for the
            // act first, and a match without them would be a wildcard.
            Owed::Nothing | Owed::AwaitFinal | Owed::AwaitTeardown => {
                return Err(Box::new(fail(format!("{owed} puts no message on the wire"))))
            }
        };
        Ok((sip_message::serialize(&message), dst))
    }
}
