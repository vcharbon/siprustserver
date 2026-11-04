//! The catalogue's plays the composed call shapes do not state: an INFO on the
//! established call, a BYE crossing an in-dialog request, and a call whose
//! service first plays an announcement from a media server.

use scenario_harness::actor::{
    phase, ActorCall, ActorScenario, ActorSpec, Barrier, CtxFeed, Disposition, Expect, Goal,
    GoalStep, LegPhase, MediaState, SettleBarrier, StateInner,
};
use scenario_harness::realcall::{CallEnv, ScenarioId};
use scenario_harness::{StepError, ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;

use crate::sweep::MEDIA_ROLE;

/// The refer key the sweep's decision authorizes a transfer to the transfer
/// target under.
pub const REFER_KEY: &str = "refer-allow-c";

/// The media server's answer.
const MEDIA_SDP: &str = "v=0\r\no=media 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

fn established(s: &StateInner) -> bool {
    s.leg_at_least("alice", LegPhase::Confirmed) && s.leg_at_least("bob", LegPhase::Confirmed)
}

/// The caller, originating toward bob, with `goals` after her INVITE.
fn caller(env: &CallEnv<'_>, goals: Vec<Goal>) -> ActorSpec {
    let mut all = vec![Goal::new(
        Barrier::None,
        GoalStep::Invite { callee: "bob", plan: Some(env.invite_plan(&["bob"])) },
    )];
    all.extend(goals);
    ActorSpec {
        role: "alice",
        agent: env.alice.clone(),
        disposition: Disposition::Caller,
        media: MediaState::offer(OFFER_SDP),
        goals: all,
        invite_targets: vec![("bob", env.bob.clone())],
        via: None,
        feed: CtxFeed::default(),
        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

/// A callee answering as `role`, with `goals`.
fn callee(
    role: &'static str,
    agent: &scenario_harness::Agent,
    disposition: Disposition,
    sdp: &'static str,
    goals: Vec<Goal>,
) -> ActorSpec {
    ActorSpec {
        role,
        agent: agent.clone(),
        disposition,
        media: MediaState::answer(sdp),
        goals,
        invite_targets: vec![],
        via: None,
        feed: CtxFeed::default(),
        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

fn call(actors: Vec<ActorSpec>) -> ActorCall {
    ActorCall::new(
        actors,
        vec![phase("established", established)],
        SettleBarrier::default_ceiling(),
        Expect::answered(),
    )
}

fn info() -> GoalStep {
    GoalStep::InDialog {
        method: InDialogMethod::Info,
        content_type: None,
        body: None,
        headers: vec![],
    }
}

/// The caller sends an INFO on the established call, then hangs up once it is
/// answered.
pub struct InfoCall;

impl ActorScenario for InfoCall {
    fn id(&self) -> ScenarioId {
        "info"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let informed = |s: &StateInner| {
            s.leg("alice").responses().iter().any(|r| r.cseq_method == "INFO" && r.status == 200)
        };
        let alice = caller(
            env,
            vec![
                Goal::new(Barrier::pred("established", established), info()).after(env.talk_time),
                Goal::new(Barrier::pred("informed", informed), GoalStep::Bye),
            ],
        );
        let bob = callee(
            "bob",
            env.bob,
            Disposition::RingThenAnswer { ring: env.ring_delay },
            ANSWER_SDP,
            vec![],
        );
        Ok(call(vec![alice, bob]))
    }
}

/// The in-dialog request a BYE crosses.
#[derive(Clone, Copy, Debug)]
pub enum Crossed {
    Reinvite,
    Update,
    Info,
    Refer,
}

/// A BYE crossing an in-dialog request: once the call is established, one
/// party sends the request and the other hangs up at the same instant, so each
/// meets the other in flight. The callee hangs up on the caller's re-INVITE,
/// UPDATE or INFO; the caller hangs up on the callee's REFER.
pub struct ByeCrossing(pub Crossed);

impl ActorScenario for ByeCrossing {
    fn id(&self) -> ScenarioId {
        match self.0 {
            Crossed::Reinvite => "reinvite_x_bye",
            Crossed::Update => "update_x_bye",
            Crossed::Info => "info_x_bye",
            Crossed::Refer => "refer_x_bye",
        }
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let gate = || Barrier::pred("established", established);
        let ring = Disposition::RingThenAnswer { ring: env.ring_delay };
        let (alice_goal, bob_goal) = match self.0 {
            Crossed::Reinvite => (GoalStep::Reinvite, GoalStep::Bye),
            Crossed::Update => (GoalStep::Update, GoalStep::Bye),
            Crossed::Info => (info(), GoalStep::Bye),
            Crossed::Refer => {
                let refer_to = env.refer_to().ok_or_else(|| StepError::UnexpectedKind {
                    who: "refer_x_bye".to_string(),
                    detail: "no transfer target bound".to_string(),
                })?;
                let authorization = env.refer_authorization(REFER_KEY);
                (GoalStep::Bye, GoalStep::Refer { refer_to, authorization })
            }
        };
        let alice = caller(env, vec![Goal::new(gate(), alice_goal).after(env.talk_time)]);
        let bob = callee(
            "bob",
            env.bob,
            ring,
            ANSWER_SDP,
            vec![Goal::new(gate(), bob_goal).after(env.talk_time)],
        );
        let mut actors = vec![alice, bob];
        if let (Crossed::Refer, Some(charlie)) = (self.0, env.charlie) {
            actors.push(callee("charlie", charlie, Disposition::Answer, ANSWER_SDP, vec![]));
        }
        Ok(call(actors))
    }
}

/// A call whose service first dials a media server for an announcement: the
/// caller hears it as early media, the media server reports the clip's end on
/// its control channel, the service releases it and dials bob, and the caller
/// hangs up once answered.
pub struct MediaLeg;

impl ActorScenario for MediaLeg {
    fn id(&self) -> ScenarioId {
        "media_leg"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let alice = caller(
            env,
            vec![Goal::new(Barrier::pred("established", established), GoalStep::Bye)
                .after(env.talk_time)],
        );
        let clip_end = Goal::new(
            Barrier::received("media_play_received", MEDIA_ROLE, "INFO"),
            GoalStep::InDialog {
                method: InDialogMethod::Info,
                content_type: Some("application/mediaservercontrol+xml".to_string()),
                body: Some(announcement::mscml::build_response(200)),
                headers: vec![],
            },
        )
        .after(env.ring_delay);
        let media = callee(
            MEDIA_ROLE,
            env.callee_agent(MEDIA_ROLE),
            Disposition::Answer,
            MEDIA_SDP,
            vec![clip_end],
        );
        let bob = callee("bob", env.bob, Disposition::Answer, ANSWER_SDP, vec![]);
        Ok(call(vec![alice, media, bob]))
    }
}
