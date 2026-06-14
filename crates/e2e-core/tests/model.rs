//! Phase D acceptance (ADR-0018): the M1 test as authored JSON — loaded,
//! validated against the shape registry, and driven through the fake infra;
//! an intentionally-incompatible case fails validation with a precise message;
//! the committed `e2e/schemas/*.schema.json` match the model (drift fails CI —
//! regenerate with `cargo run -p xtask -- e2e-schema`).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use e2e_core::model::{self, ModelError};
use e2e_core::{BasicCall, CallflowShape, EndpointConfig, FakeLsbcB2bua, InfraShape, shapes};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn fake_cfg() -> EndpointConfig {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", "127.0.0.1:5060"),
        ("bob1", "127.0.0.1:5070"),
        ("lb", "127.0.0.1:5080"),
        ("b2bua", "127.0.0.1:5090"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
    .collect();
    EndpointConfig {
        schema: None,
        infra_shape: "fake-lsbc-b2bua".into(),
        roles,
        recv_timeout_ms: 2_000,
        transit_delay_ms: 0,
    }
}

/// The authored M1 case loads, validates against the registry, and its input
/// drives the same fake run the hand-written portability test performs.
#[tokio::test(start_paused = true)]
async fn authored_json_case_loads_validates_and_runs() {
    let path = workspace_root().join("e2e/cases/basic-call-identity.json");
    let case = model::load_test_case(&path).expect("the committed case file loads");
    assert_eq!(case.id, "basic-call-identity");

    let shapes = shapes::registry();
    let check_sets = model::load_check_sets(&workspace_root().join("e2e/checksets"))
        .expect("check-set store loads (missing dir = empty)");
    model::validate_case(&case, &shapes, &check_sets).expect("the committed case validates");

    // Drive the case's input through the shape it declares, over the fake infra.
    let shape = shapes.get(case.compatible_shapes[0].as_str()).unwrap();
    let mut rt = FakeLsbcB2bua.build("basic-call/fake/json-case", &fake_cfg()).await;
    let lb_vip = rt.lb_vip;
    shape.run(&mut rt, &case.input).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");

    assert!(report.passed(), "run must pass the RFC hard gate");

    // …and the case's own declarative checks evaluate green (Phase E).
    let bindings = e2e_core::checks::Bindings { input: &case.input, lb_vip };
    let verdicts = e2e_core::checks::evaluate_case(&case, &check_sets, &report, &bindings);
    assert!(!verdicts.is_empty(), "the committed case must carry checks");
    for v in &verdicts {
        assert!(v.passed, "{}.{} failed: {} (actual {:?})", v.on, v.field, v.detail, v.actual);
    }

    let bob1: SocketAddr = "127.0.0.1:5070".parse().unwrap();
    let lb: SocketAddr = "127.0.0.1:5080".parse().unwrap();
    let invite_to_bob1 = report
        .entries()
        .into_iter()
        .find(|e| {
            e.from == lb && e.to == bob1 && String::from_utf8_lossy(&e.raw).starts_with("INVITE")
        })
        .expect("an INVITE delivered lb→bob1");
    let text = String::from_utf8_lossy(&invite_to_bob1.raw);
    assert!(
        text.contains("+33123456789"),
        "the JSON-authored From user-part must survive to bob1's INVITE:\n{text}"
    );
}

fn parse_case(json: &str) -> model::TestCase {
    serde_json::from_str(json).expect("test JSON parses")
}

fn validation_problems(case: &model::TestCase) -> Vec<String> {
    match model::validate_case(case, &shapes::registry(), &BTreeMap::new()) {
        Err(ModelError::Invalid(problems)) => problems,
        other => panic!("expected validation failure, got {other:?}"),
    }
}

/// An anchor the shape does not publish is an incompatibility, named precisely.
#[test]
fn unpublished_anchor_fails_validation_with_precise_message() {
    let case = parse_case(
        r#"{
            "id": "bad-anchor",
            "compatibleShapes": ["basic-call"],
            "checks": [
                { "on": "bob1.prack", "checks": [ { "field": "rack", "op": "exists" } ] }
            ]
        }"#,
    );
    let problems = validation_problems(&case);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].contains("incompatible with shape \"basic-call\"")
            && problems[0].contains("\"prack\"")
            && problems[0].contains("initialInvite"),
        "message must name the shape, the anchor, and what IS published: {}",
        problems[0]
    );
}

/// Unknown shape ids, non-canonical anchors, malformed selectors, missing
/// check-set ids and op/value mismatches are all reported (not just the first).
#[test]
fn every_validation_problem_is_reported() {
    let case = parse_case(
        r#"{
            "id": "many-problems",
            "compatibleShapes": ["no-such-shape"],
            "checkSets": ["no-such-set"],
            "checks": [
                { "on": "bob1.nonsenseAnchor", "checks": [ { "field": "from", "op": "exists" } ] },
                { "on": "justoneword", "checks": [ { "field": "from", "op": "regex" } ] },
                { "on": "bob1.bye", "checks": [ { "field": "to", "op": "absent", "value": "x" } ] }
            ]
        }"#,
    );
    let problems = validation_problems(&case);
    let all = problems.join("\n");
    assert!(all.contains("unknown Callflow shape \"no-such-shape\""), "{all}");
    assert!(all.contains("unknown check set \"no-such-set\""), "{all}");
    assert!(all.contains("\"nonsenseAnchor\"") && all.contains("canonical"), "{all}");
    assert!(all.contains("\"justoneword\"") && all.contains("<agent>.<anchor>"), "{all}");
    assert!(all.contains("op Regex requires a value"), "{all}");
    assert!(all.contains("op Absent takes no value"), "{all}");
}

/// A shape's `required_input` gates compatibility on the case's input.
#[test]
fn missing_required_input_is_incompatible() {
    struct NeedsTarget;
    #[async_trait::async_trait(?Send)]
    impl CallflowShape for NeedsTarget {
        fn id(&self) -> &str {
            "needs-target"
        }
        fn anchors(&self) -> &[e2e_core::Anchor] {
            &[]
        }
        fn required_input(&self) -> &[&str] {
            &["from", "rerouteTarget"]
        }
        async fn run(&self, _rt: &mut e2e_core::InfraRuntime, _input: &e2e_core::model::Input) {
            unreachable!("validation-only shape")
        }
    }
    let mut shapes: BTreeMap<String, Box<dyn CallflowShape>> = BTreeMap::new();
    shapes.insert("needs-target".into(), Box::new(NeedsTarget));

    let case = parse_case(
        r#"{
            "id": "missing-required",
            "compatibleShapes": ["needs-target"],
            "input": { "core": { "from": "sip:a@b" } }
        }"#,
    );
    let problems = match model::validate_case(&case, &shapes, &BTreeMap::new()) {
        Err(ModelError::Invalid(problems)) => problems,
        other => panic!("expected validation failure, got {other:?}"),
    };
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].contains("required input field \"rerouteTarget\" is missing"),
        "{}",
        problems[0]
    );

    // Supplying it as an extras key satisfies the requirement.
    let case = parse_case(
        r#"{
            "id": "has-required",
            "compatibleShapes": ["needs-target"],
            "input": {
                "core": { "from": "sip:a@b" },
                "extras": { "rerouteTarget": "sip:bob2@host" }
            }
        }"#,
    );
    model::validate_case(&case, &shapes, &BTreeMap::new()).expect("extras satisfy required input");
}

/// An Endpoint config authored for one infra cannot build another (fail loud).
#[tokio::test(start_paused = true)]
#[should_panic(expected = "endpoint config is for infra")]
async fn endpoint_config_bound_to_wrong_infra_panics() {
    let mut cfg = fake_cfg();
    cfg.infra_shape = "real-loopback-direct".into();
    let _ = FakeLsbcB2bua.build("mismatch", &cfg).await;
}

/// The committed schemas match the model — regenerate with
/// `cargo run -p xtask -- e2e-schema` when this fails.
#[test]
fn committed_schemas_are_current() {
    let dir = workspace_root().join("e2e/schemas");
    for (stem, schema) in model::schemas() {
        let path = dir.join(format!("{stem}.schema.json"));
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "read {} ({e}); run `cargo run -p xtask -- e2e-schema`",
                path.display()
            )
        });
        let current = serde_json::to_string_pretty(&schema).unwrap();
        assert_eq!(
            committed.trim_end(),
            current,
            "{stem}.schema.json drifted from the model; run `cargo run -p xtask -- e2e-schema`"
        );
    }
}

/// BasicCall publishes what the committed case binds (sanity on the contract
/// between the Rust shape and the canonical vocabulary).
#[test]
fn basic_call_publishes_the_documented_anchors() {
    use e2e_core::Anchor;
    let anchors = BasicCall.anchors();
    for a in [Anchor::InitialInvite, Anchor::Answer, Anchor::Ack, Anchor::Bye] {
        assert!(anchors.contains(&a), "basic-call must publish {:?}", a.as_str());
    }
    assert_eq!(Anchor::parse("initialInvite"), Some(Anchor::InitialInvite));
    assert_eq!(Anchor::parse("nope"), None);
}
