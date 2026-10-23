//! The lane's seeds: calls the B2BUA ran on the paused clock, each encoded
//! as its replication writes it. A forked call is snapshotted live while two
//! forks ring, once one answered and while its BYE is out, then as it
//! terminated; the first cells of the call-shape generator's fixed sample
//! seed one terminated call each; one answered snapshot carries opaque
//! per-service JSON, so the decoder's untyped recursion is reached.

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::{Call, CallBodyCodec, DialogState, EndingBy, MsgpackCodec};
use callshapes::generator;
use callshapes::shapes::default_binder;
use scenario_harness::actor::ScenarioVerdict;
use scenario_harness::realcall::{run_actor_collecting, CallEnv};
use scenario_harness::Harness;

/// One seed: the call and its body.
pub struct Seed {
    pub name: String,
    pub call: Call,
    pub bytes: Vec<u8>,
}

impl Seed {
    fn of(name: impl Into<String>, call: Call) -> Self {
        let bytes = MsgpackCodec::new().encode(&call).expect("a call the B2BUA ran encodes");
        Seed { name: name.into(), call, bytes }
    }
}

/// How many cells of the generator's fixed sample seed the lane.
const SHAPES: usize = 12;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const CALL_ID: &str = "snapshot-lane@127.0.0.1";
const FROM_TAG: &str = "alice-tag";

/// Every seed, on a paused runtime of its own.
pub fn all() -> Vec<Seed> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a paused runtime");
    rt.block_on(async {
        let mut seeds = forked_call().await;
        seeds.extend(call_shapes().await);
        let answered = seeds.iter().find(|s| s.name == "forked/answered").expect("answered");
        seeds.push(with_service_json(&answered.call));
        seeds
    })
}

/// A call whose callee forks twice and answers on the second fork; the
/// caller hangs up.
async fn forked_call() -> Vec<Seed> {
    let h = Harness::with_transit_delay("snapshot-lane-forked", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob.addr().port())
        .keep_terminated_calls()
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let live = || b2bua.live_call(&call_ref).expect("the call is live");
    let mut seeds = Vec::new();

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("fork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("fork2").await;
    call.expect(180).await;
    seeds.push(Seed::of("forked/ringing", live()));

    uas.respond(200, "OK").with_to_tag("fork2").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let acked = |c: &Call| {
        c.b_legs
            .iter()
            .all(|l| l.dialogs.iter().all(|d| call::helpers::awaited_ack_cseq(d).is_none()))
    };
    settle_until(|| b2bua.live_call(&call_ref).is_some_and(|c| acked(&c))).await;
    seeds.push(Seed::of("forked/answered", live()));

    let mut bye = dialog.bye().await;
    let mut bye_at_bob = bob.receive("BYE").await;
    let ending = |c: &Call| {
        c.b_legs
            .iter()
            .any(|l| l.dialogs.iter().any(|d| d.state == DialogState::Ending(EndingBy::Bye)))
    };
    settle_until(|| b2bua.live_call(&call_ref).is_some_and(|c| ending(&c))).await;
    seeds.push(Seed::of("forked/ending", live()));

    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    seeds.extend(b2bua.terminated_calls().into_iter().map(|c| Seed::of("forked/terminated", c)));
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert!(report.passed(), "the forked call's RFC audit failed");
    seeds
}

/// The terminated call of each of the first [`SHAPES`] cells of the fixed
/// sample.
async fn call_shapes() -> Vec<Seed> {
    let mut seeds = Vec::new();
    for cell in generator::sample().into_iter().take(SHAPES) {
        let scene = B2buaScene::with_b2bua(&format!("snapshot-lane cell {cell}"), |bob_port| {
            B2buaSut::route_all_to("127.0.0.1", bob_port).keep_terminated_calls()
        })
        .await;
        let env = CallEnv::for_functional(
            &scene.alice,
            &scene.bob,
            None,
            scene.b2bua.addr,
            "X-Loadgen-Id",
            "cell-tok",
        );
        let plan = generator::plan(&cell, default_binder());
        if let ScenarioVerdict::Failure { at, cause } = run_actor_collecting(&plan, &env).await {
            panic!("cell {cell}: failed at {at}: {cause:?}");
        }
        settle_until(|| scene.b2bua.is_reaped()).await;
        let name = format!("shape/{cell}");
        seeds.extend(scene.b2bua.terminated_calls().into_iter().map(|c| Seed::of(name.clone(), c)));
        let report = scene.finish().await;
        assert!(report.passed(), "cell {cell}: the RFC audit failed");
    }
    seeds
}

/// `call` carrying a service's opaque state a few levels deep, on the call
/// and on its caller's leg.
fn with_service_json(call: &Call) -> Seed {
    let state = serde_json::json!({
        "phase": "connected",
        "history": [{"at": 1_000, "event": {"kind": "answer", "codes": [200, 180]}}],
        "limits": {"max": 3, "per": {"minute": 2}},
    });
    let a_leg = call.a_leg.leg_id.clone();
    let call = call::helpers::set_call_ext(call.clone(), "lane-service", Some(state.clone()));
    let call = call::helpers::set_leg_ext(call, &a_leg, "lane-service", state);
    Seed::of("forked/answered+service-json", call)
}
