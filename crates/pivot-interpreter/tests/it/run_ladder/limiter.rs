//! Admission control: a limited second call refused before any b-leg.

use super::*;

/// The address the lane's limiter server answers at, on its own simulated HTTP
/// fabric — a private network the SIP transport never sees.
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// A real `LimiterServer` over `net`, plus the store its counters live in, so a
/// rung can read what the system actually held rather than inferring it from
/// the wire. The handle keeps the server task alive for the run.
async fn serve_limiter(
    net: &SimulatedHttpNetwork,
) -> (Arc<CallStore>, Box<dyn http_net::HttpServerHandle>) {
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = net
        .serve(LIMITER_ADDR.parse().expect("the limiter address parses"), server)
        .await
        .expect("the limiter binds");
    (store, handle)
}

/// The scene a LIMITED document replays against: the deployed-engine decision —
/// which honours the lane's `X-Api-Call` egress directive, its admission entry
/// included — over the system's own `HttpCallLimiter` pointed at `net`, whose
/// `store` the reaped check reads.
async fn limited_scene(
    name: &str,
    net: &SimulatedHttpNetwork,
    store: Arc<CallStore>,
) -> B2buaScene {
    let client: Arc<dyn CallLimiter> = Arc::new(HttpCallLimiter::new(
        Arc::new(net.clone()),
        LIMITER_ADDR.parse().expect("the limiter address parses"),
        std::time::Duration::from_millis(150),
    ));
    B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::route_api_call("127.0.0.1", bob_port).limiter(client).limiter_store(store)
    })
    .await
}

/// The LIMITER rung: a second call arrives while the first holds the only
/// admission slot, and the platform's own admission control refuses it.
///
/// The shape the draft exists for is a refusal that never becomes a call: the
/// caller of the second call gets its final (ADR-0022 — once 100 Trying is out,
/// a final is owed), no b-leg is ever dialled, both calls are billed, and the
/// admitted call's slot comes back. The document says all four — the 486 as a
/// step, the absent b-leg as a `background` counter bounded `exactly: 0`, two
/// CDRs as a postcondition — and the rung reads the limiter's own store beside
/// them, so a green run cannot mean "the callee happened to be busy".
///
/// The cap is what refuses: raise it to 2 and the second call is admitted and
/// dialled — at ITS OWN callee, because each call carries its own egress
/// directive (§4.3) — and the `exactly: 0` counter on that callee is what fails
/// the run. The counter counts because the lane could have reached the party it
/// counts (§5.1); on a lane that could not, it would be a check that cannot
/// fail.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_six_a_limited_second_call_is_refused_before_any_b_leg() {
    let Some(case) = case_dir("admission-exceeded-two-calls") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let net = SimulatedHttpNetwork::new();
    let (store, _limiter) = serve_limiter(&net).await;
    let scene = limited_scene("pivot-admission", &net, store).await;
    let extra = BTreeMap::from([
        ("uac2".to_string(), scene.h.agent("uac2", "127.0.0.1:5061").await),
        ("uas2".to_string(), scene.h.agent("uas2", &format!("127.0.0.1:{IDLE_PORT}")).await),
    ]);
    let admission = Admission { id: "trunk-A".to_string(), limit: 1 };
    let directives = LaneDirectives { admission: Some(admission), ..Default::default() };
    let (outcome, dir) = replay_directed(&scene, case, extra, directives).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=17).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    // EACH call carries its own egress directive, naming its own callee's socket
    // (§4.3). Without that, the absence counter below would watch an endpoint
    // this lane could never dial, and a check that cannot fail is not evidence.
    let run_config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("run-config.json")).unwrap())
            .expect("the bundle's run configuration parses");
    let directive = |call: &str| {
        run_config["call_headers"][call]["X-Api-Call"].as_str().unwrap_or_default().to_string()
    };
    assert!(
        directive("c1").contains(&format!("\"port\":{BOB_PORT}")),
        "the first call is dialled at its own callee: {}",
        directive("c1")
    );
    assert!(
        directive("c2").contains(&format!("\"port\":{IDLE_PORT}")),
        "the second call is dialled at ITS own callee, not the first one's: {}",
        directive("c2")
    );

    let legs = outcome.recording.legs();
    // The refused caller got its final, and it is the document's own step.
    let refusal = legs["C"]
        .iter()
        .find(|m| text(m).starts_with("SIP/2.0 486"))
        .expect("the second caller was answered");
    assert_eq!(refusal.step.as_deref(), Some("s12"));
    // And it was refused BEFORE any leg: the second call's callee heard nothing
    // at all, which is the whole difference between admission control and a
    // busy destination. The document says the same thing as a `background`
    // counter bounded `exactly: 0`, and a green run is that counter holding.
    assert!(legs.get("D").is_none_or(|messages| messages.is_empty()), "{legs:#?}");
    assert!(
        !outcome.verdict.failures.iter().any(|f| matches!(f, Failure::BackgroundCount { .. })),
        "{:#?}",
        outcome.verdict.failures
    );

    // Both calls were BILLED, and the document says so with the only per-record
    // scoping `{count, checks}` has: the count. Which record carries what is
    // unaskable there, so the rung reads it off this lane's own CDR — the
    // draft's event checks carry `cdr-vocabulary`, so whatever they find is
    // recorded rather than gating (§9.1).
    let records = scene.b2bua.cdr_records();
    assert_eq!(records.len(), 2, "{records:#?}");
    let refused = records
        .iter()
        .find(|record| record.b_legs.is_empty())
        .expect("the refused call was billed with no b-leg");
    assert!(
        refused.events.iter().any(|event| event.status_code == Some(486)),
        "the refusal is on the record: {:#?}",
        refused.events
    );
    let answered = records
        .iter()
        .find(|record| !record.b_legs.is_empty())
        .expect("the admitted call was billed");
    assert!(
        answered.events.iter().any(|event| event.event_type == CdrEventType::Answer),
        "{:#?}",
        answered.events
    );

    // And the residue, if any, is CDR vocabulary, and nothing gates.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );

    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}
