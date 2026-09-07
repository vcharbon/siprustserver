//! Shared plan/template builders for the actor test suite: captured-
//! message stand-ins (`invite_template` / `response_template`) and the
//! caller/scripted [`ActorSpec`] shorthands.

use crate::actor::*;
use crate::{ANSWER_SDP, OFFER_SDP};
use sip_message::{MessageTemplate, Method, TemplateHeader};
/// A captured-INVITE stand-in: frozen extra headers + an SDP offer body.
pub(super) fn invite_template(extra: &[(&str, &str)]) -> MessageTemplate {
    let mut headers = vec![TemplateHeader::frozen("Content-Type", "application/sdp")];
    for (n, v) in extra {
        headers.push(TemplateHeader::frozen(*n, *v));
    }
    MessageTemplate::request(Method::Invite, headers, OFFER_SDP.as_bytes().to_vec())
}

/// A captured-response stand-in — optionally carrying the answer SDP.
pub(super) fn response_template(status: u16, reason: &str, sdp: bool) -> MessageTemplate {
    if sdp {
        MessageTemplate::response(
            status,
            reason,
            vec![TemplateHeader::frozen("Content-Type", "application/sdp")],
            ANSWER_SDP.as_bytes().to_vec(),
        )
    } else {
        MessageTemplate::response(status, reason, vec![], Vec::new())
    }
}

/// A plain caller spec with the given goals (offer media, no plan/via).
pub(super) fn caller_spec(role: &'static str, agent: &crate::Agent, callee: (&'static str, crate::Agent), goals: Vec<Goal>) -> ActorSpec {
    ActorSpec {
        role,
        agent: agent.clone(),
        disposition: Disposition::Caller,
        media: MediaState::offer(OFFER_SDP),
        goals,
        invite_targets: vec![callee],
        via: None,
        feed: CtxFeed::default(),
    
        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

/// A Scripted callee spec with the given goals (answer media).
pub(super) fn scripted_spec(role: &'static str, agent: &crate::Agent, goals: Vec<Goal>) -> ActorSpec {
    ActorSpec {
        role,
        agent: agent.clone(),
        disposition: Disposition::Scripted,
        media: MediaState::answer(ANSWER_SDP),
        goals,
        invite_targets: vec![],
        via: None,
        feed: CtxFeed::default(),
    
        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

pub(super) fn established_phase() -> BarrierPhase {
    phase("established", |s| {
        s.leg_at_least("alice", LegPhase::Confirmed)
            && s.leg_at_least("bob", LegPhase::Confirmed)
    })
}
