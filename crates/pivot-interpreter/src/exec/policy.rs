//! The `background` policy that answers an arrival outside the flow, and its answer.

use pivot_schema::bundle::{Dir, Failure};
use sip_message::SipMessage;
use tokio::time::Instant;

use super::compose::reason_for;
use super::unscripted::via_target;
use super::Runner;
use crate::gate::{self, Inbound};

impl<'a, 'p> Runner<'a, 'p> {
    /// Whether the FLOW is waiting for this datagram on `leg`: a frontier
    /// expect whose budget has opened and whose discriminator it satisfies.
    ///
    /// A `background` policy answers traffic OUTSIDE the flow (§5.1), and a step
    /// whose window is open is the flow. Both halves are load-bearing on a
    /// policy-answered method the system also RELAYS: the relayed request is
    /// call behaviour a scripted expect owns, while the same method arriving
    /// before that expect's window opens is the replaying SUT's own audit,
    /// whose cadence is a deployment parameter the flow must never gate on.
    pub(super) fn flow_awaits(&self, leg: &str, inbound: &Inbound) -> bool {
        let now = Instant::now();
        self.instance.cursor().frontier().iter().any(|id| {
            let Some(step) = self.instance.plan().step(id) else { return false };
            step.is_expect()
                && step.leg == leg
                && self.armed_at.get(id).is_some_and(|opened| *opened <= now)
                && gate::discriminates(step, inbound).matches()
        })
    }

    /// Answer a background policy's message. It is answered and recorded; the
    /// flow never sees it.
    pub(super) async fn answer_background(
        &mut self,
        actor: &str,
        leg: &str,
        message: &SipMessage,
        status: u16,
    ) -> bool {
        let SipMessage::Request(request) = message else { return true };
        let Some(agent) = self.lane.agents.get(actor).cloned() else { return true };
        let response = sip_message::generators::generate_response(
            request,
            status,
            reason_for(status),
            &sip_message::generators::GenerateResponseOpts {
                to_tag: Some(format!("{leg}-bg")),
                ..Default::default()
            },
        );
        let wire = sip_message::serialize(&SipMessage::Response(response));
        let (dst, note) = match via_target(request) {
            Some(addr) => (addr, "background answer"),
            None => (
                self.lane.route_target,
                "background answer; the request named no reachable Via sent-by, so it is \
                 addressed at the lane's route target",
            ),
        };
        match agent.try_send_datagram(&wire, dst).await {
            Ok(()) => {
                self.instance.recording().push(
                    leg,
                    Dir::Out,
                    self.now_us(),
                    wire,
                    None,
                    Some(note),
                );
                true
            }
            Err(e) => {
                self.instance.fail(Failure::SendFailed {
                    step: "(background)".into(),
                    leg: leg.to_string(),
                    detail: e.to_string(),
                });
                false
            }
        }
    }

    /// The background policy that answers `method` at the endpoint `actor` sits
    /// on. A loopback endpoint hosts several actors on one socket, so the policy
    /// belongs to the ENDPOINT's actors, not to whichever one happened to pump.
    pub(super) fn background_policy(
        &self,
        actor: &str,
        method: &str,
        leg: Option<&str>,
    ) -> Option<(usize, crate::background::Policy)> {
        // The LEG's own actor first, where the datagram rides one: a loopback
        // endpoint hosts two actors on one socket, and crediting both their
        // policies to whichever is declared first makes one counter count twice
        // and the other never.
        let owner = leg.and_then(|leg| self.instance.plan().actor_of_leg(leg));
        if let Some((index, policy)) =
            owner.and_then(|owner| self.instance.background().policy_for(owner, method))
        {
            return Some((index, policy.clone()));
        }
        // Otherwise any actor on the receiving endpoint: out-of-dialog traffic
        // belongs to the socket, not to a dialog.
        let endpoint = self.instance.plan().actor(actor).map(|a| a.endpoint.as_str())?;
        self.instance
            .plan()
            .actors()
            .values()
            .filter(|candidate| candidate.endpoint == endpoint)
            .find_map(|candidate| self.instance.background().policy_for(&candidate.id, method))
            .map(|(index, policy)| (index, policy.clone()))
    }
}
