//! Which leg a datagram rides, and the dialog facts each leg publishes.

use scenario_harness::Agent;
use sip_message::Method;

use super::Runner;
use crate::gate::{self, GateVerdict, Inbound};
use crate::plan::CompiledStep;

impl<'a, 'p> Runner<'a, 'p> {
    /// The fork gate for `step`, under whichever §6.1 reading the plan gave its
    /// id: an ANSWERED fork gates on the tag this run minted, an OBSERVED one
    /// on what the leg has learned so far. Pure either way — binding is
    /// `deliver`'s, on consumption.
    pub(super) fn rides_fork(&self, step: &CompiledStep, inbound: &Inbound) -> GateVerdict {
        match self.early.tag_of(step) {
            Some(tag) => gate::rides_early_dialog(step, inbound, Some(tag)),
            None => gate::rides_observed_fork(step, inbound, &self.learned),
        }
    }

    /// The To-tag of the fork a step names, under whichever §6.1 reading the
    /// plan gave its id: the tag this run MINTED for an ANSWERED fork, the tag
    /// the leg LEARNED for an OBSERVED one — absent there until an arrival
    /// taught it.
    pub(super) fn fork_tag_of(&self, step: &CompiledStep) -> Option<&str> {
        self.early
            .tag_of(step)
            .or_else(|| step.early.as_deref().and_then(|early| self.learned.tag(&step.leg, early)))
    }

    /// The leg carrying `call_id`, where one already does.
    pub(super) fn leg_by_call_id(&self, call_id: &str) -> Option<String> {
        self.stacks.iter().find(|(_, stack)| stack.call_id() == call_id).map(|(leg, _)| leg.clone())
    }

    /// Which leg an arriving datagram rides, or why none does.
    ///
    /// A claim is scoped to the SOCKET the INVITE arrived on: `actor` is the one
    /// whose agent surfaced it, and only the legs on that actor's endpoint may
    /// claim it. Offering it to every endpoint would let an `arrival-order`
    /// candidate on another socket steal a dialog it never received.
    pub(super) fn leg_of(&mut self, actor: &str, inbound: &Inbound) -> Result<String, String> {
        if let Some(leg) = self.leg_by_call_id(&inbound.call_id) {
            return Ok(leg);
        }
        if inbound.method.as_deref().map(Method::from_wire) != Some(Method::Invite) {
            return Err(format!(
                "no leg carries Call-ID {:?}, and only an initial INVITE may open one",
                inbound.call_id
            ));
        }
        let endpoint = self
            .instance
            .plan()
            .actor(actor)
            .map(|a| a.endpoint.clone())
            .ok_or_else(|| format!("actor {actor:?} is not declared, so it hosts no claim"))?;
        let ruri_user = inbound.ruri_user.clone().unwrap_or_default();
        self.instance.claim(&endpoint, &ruri_user, inbound).map_err(|e| e.to_string())
    }

    /// Publish the stack's dialog facts into the state accessors read — the
    /// leg's own, and each fork's where the leg is ringing several (§8.1).
    pub(super) fn sync_leg_state(&mut self, leg: &str) {
        let Some(stack) = self.stacks.get(leg) else { return };
        let forks: Vec<(String, u32)> = self
            .early
            .iter()
            .filter(|(owner, _, _)| *owner == leg)
            .filter_map(|(_, id, tag)| stack.early_rseq(tag).map(|rseq| (id.to_string(), rseq)))
            .collect();
        let call_id = stack.call_id().to_string();
        let local_tag = stack.local_tag().to_string();
        let remote_tag = stack.remote_tag().map(str::to_string);
        let remote_target = stack.remote_target().to_string();
        let route_set = stack.route_set().to_vec();
        let cseq_local = stack.local_cseq();
        let cseq_remote = stack.remote_cseq();
        let rseq = stack.rseq();
        let state = self.instance.leg_mut(leg);
        state.call_id = Some(call_id);
        state.local_tag = Some(local_tag);
        state.remote_tag = remote_tag;
        state.remote_target = Some(remote_target);
        state.route_set = route_set;
        state.cseq_local = cseq_local;
        state.cseq_remote = cseq_remote;
        state.rseq = rseq;
        for (id, rseq) in forks {
            self.instance.record_early_rseq(&id, rseq);
        }
    }

    /// The agent that plays `leg`, if the lane bound one.
    pub(super) fn agent_of_leg(&self, leg: &str) -> Option<Agent> {
        let actor = self.instance.plan().actor_of_leg(leg)?;
        self.lane.agents.get(actor).cloned()
    }
}
