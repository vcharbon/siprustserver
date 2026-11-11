//! An in-dialog request still awaiting its final when its call ends is judged
//! by its client transaction (RFC 3261 §17.1.2), on the paused-clock rig with
//! the retransmission engine on: the caller's INFO draws the callee's 200,
//! which the B2BUA relays while the callee's BYE crosses it, and that 200 is
//! lost on its way to the caller. The call ends on the BYE; the INFO's
//! retransmission draws the 200 again from the B2BUA's server transaction, and
//! the call's record names no failed request.

use std::sync::Arc;

use b2bua_harness::settle_until;
use e2e_model::ShapeDescriptor;
use loadgen::outcome::OutcomeSink;
use loadgen::{
    CallTuning, Driver, DropDir, MixEntry, Reporter, ReporterCfg, ResultClass, ScenarioInputs,
    ShapeRegistry, TargetedDrop,
};
use scenario_harness::actor::{
    phase, ActorCall, ActorScenario, ActorSpec, Barrier, CtxFeed, Disposition, Expect, Goal,
    GoalStep, LegPhase, MediaState, SettleBarrier, StateInner,
};
use scenario_harness::realcall::{CallEnv, ScenarioId};
use scenario_harness::{StepError, ANSWER_SDP, OFFER_SDP};
use serde_json::Value;
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::fake_rig::{cfg, setup_fake};

const SHAPE: &str = "info_bye_crossing";

fn established(s: &StateInner) -> bool {
    s.leg_at_least("alice", LegPhase::Confirmed) && s.leg_at_least("bob", LegPhase::Confirmed)
}

/// The caller sends an INFO once the call is established; the callee answers
/// it and hangs up as soon as it arrived.
struct InfoByeCrossing;

impl ActorScenario for InfoByeCrossing {
    fn id(&self) -> ScenarioId {
        SHAPE
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let info = GoalStep::InDialog {
            method: InDialogMethod::Info,
            content_type: None,
            body: None,
            headers: vec![],
        };
        let alice = ActorSpec {
            role: "alice",
            agent: env.alice.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            goals: vec![
                Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(env.invite_plan(&["bob"])) },
                ),
                Goal::new(Barrier::pred("established", established), info),
            ],
            invite_targets: vec![("bob", env.bob.clone())],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: None,
        };
        let bob = ActorSpec {
            role: "bob",
            agent: env.bob.clone(),
            disposition: Disposition::Answer,
            media: MediaState::answer(ANSWER_SDP),
            goals: vec![Goal::new(
                Barrier::received("info_received", "bob", "INFO"),
                GoalStep::Bye,
            )],
            invite_targets: vec![],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: None,
        };
        Ok(ActorCall::new(
            vec![alice, bob],
            vec![phase("established", established)],
            SettleBarrier::default_ceiling(),
            Expect::answered(),
        ))
    }
}

/// The instants (ms) the B2BUA received the caller's INFO, each copy.
fn caller_infos(h: &scenario_harness::Harness, b2bua: std::net::SocketAddr) -> Vec<u64> {
    h.wire_entries()
        .iter()
        .filter(|e| e.to == b2bua)
        .filter(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Request(r)) => {
                r.method() == Method::Info && r.call_id().to_string().contains(".run-1@")
            }
            _ => false,
        })
        .map(|e| e.sent_ms)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_info_whose_final_is_lost_as_a_bye_crosses_it_is_answered_by_its_retransmission() {
    let (h, b2bua, core, transport) = setup_fake(7900).await;
    let mut registry = ShapeRegistry::with_defaults();
    registry.register(ShapeDescriptor::new(SHAPE).load_shared(Arc::new(InfoByeCrossing)));
    let mix = MixEntry::by_id(&registry, SHAPE, &ScenarioInputs::default(), 1.0).unwrap();
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let file = std::env::temp_dir().join(format!("loadgen-pending-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let sink = Arc::new(OutcomeSink::new("run-1", "loadgen", "main", &file, None).unwrap());
    let mut c = cfg(b2bua.addr, 1.0, 1, 4, 0x0823);
    c.default_tuning = CallTuning {
        retransmit: true,
        drop_nth: Some(TargetedDrop::final_response("INFO", 1, DropDir::Inbound)),
        ..CallTuning::default()
    };
    let driver = Driver::new(c, vec![mix], reporter.clone(), transport).with_outcomes(sink.clone());
    driver.run().await;
    sink.close();
    let records: Vec<Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("one JSON record per line"))
        .collect();
    let _ = std::fs::remove_file(&file);

    assert_eq!(reporter.count(SHAPE, &ResultClass::Ok), 1, "{}", reporter.render_prometheus());
    assert_eq!(records.len(), 1, "one call placed");
    let r = &records[0];
    assert_eq!(r["ending"], "answered", "{r}");
    assert_eq!(r["failures"], serde_json::json!([]), "no failed request: {r}");
    let received: Vec<&str> =
        r["received"].as_array().unwrap().iter().filter_map(|q| q["method"].as_str()).collect();
    assert_eq!(received, ["BYE"], "the callee's BYE ended the call: {r}");
    let infos = caller_infos(&h, b2bua.addr);
    assert_eq!(infos.len(), 2, "the INFO was retransmitted once: {infos:?}");
    let ended = r["ended_at_ms"].as_u64().unwrap();
    assert!(ended < infos[1], "the call ended before the retransmission: {ended} {infos:?}");

    settle_until(|| core.registry_size() == 0).await;
    h.advance(std::time::Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
