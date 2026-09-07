//! Plan compilation: every document the schema crate ships compiles, and every
//! refusal the interpreter states is stated by NAME.
//!
//! The corpus half is deployment-neutral: it walks `pivot-schema`'s own
//! fixtures, and `PIVOT_DOC_DIRS` (a `:`-separated list) points it at any other
//! document set — which is how a deployment gates its own scenario library
//! through the same compiler.

use std::path::{Path, PathBuf};

use pivot_interpreter::plan::{Plan, PlanError};
use pivot_schema::PivotV3;

/// Every `.json` document under `dir`, one directory deep as well as flat: a
/// case directory holds `<case-id>/scenario.json`.
fn documents(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let scenario = path.join("scenario.json");
            if scenario.is_file() {
                out.push(scenario);
            }
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn corpus() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../pivot-schema/tests/fixtures")];
    if let Ok(extra) = std::env::var("PIVOT_DOC_DIRS") {
        roots.extend(extra.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    roots.iter().flat_map(|r| documents(r)).collect()
}

#[test]
fn every_document_in_the_corpus_compiles_to_a_plan() {
    let paths = corpus();
    assert!(!paths.is_empty(), "the corpus is empty; the fixture root moved");
    let mut refused = Vec::new();
    for path in &paths {
        let text = std::fs::read_to_string(path).expect("a corpus document reads");
        let document = match PivotV3::from_json(&text) {
            Ok(d) => d,
            Err(e) => {
                refused.push(format!("{}: does not parse: {e}", path.display()));
                continue;
            }
        };
        if let Err(errors) = Plan::compile(document) {
            let detail: Vec<String> = errors.iter().map(ToString::to_string).collect();
            refused.push(format!("{}: {}", path.display(), detail.join("; ")));
        }
    }
    assert!(refused.is_empty(), "{} document(s) refused:\n{}", refused.len(), refused.join("\n"));
    eprintln!("compiled {} document(s)", paths.len());
}

#[test]
fn a_compiled_plan_indexes_every_step_leg_and_actor() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../pivot-schema/tests/fixtures/authored-cancel-race.v3.json");
    let document = PivotV3::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let flow_steps = document.steps().len();
    let plan = Plan::compile(document).expect("the fixture compiles");
    assert_eq!(plan.steps().len(), flow_steps, "one compiled step per flow step");
    for step in plan.steps() {
        assert!(plan.leg(&step.leg).is_some(), "step {} rides a declared leg", step.id);
        let actor = plan.actor_of_leg(&step.leg).expect("every leg names an actor");
        assert!(plan.actor(actor).is_some());
        assert!(plan.call_of_leg(&step.leg).is_some(), "every leg belongs to a call");
    }
    // Document order is dense and follows the flow, blocks included.
    let orders: Vec<usize> = plan.steps().iter().map(|s| s.order).collect();
    assert_eq!(orders, (0..flow_steps).collect::<Vec<_>>());
}

/// Each call's DIAL, which is where a per-call lane directive lands (§4.3).
///
/// A two-call document has two of them, one per caller leg, and neither is the
/// other's: that is the whole point of keying a directive by call id.
#[test]
fn every_call_names_the_step_that_dials_it_and_no_other() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two-calls.v3.json");
    let document = PivotV3::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let plan = Plan::compile(document).expect("the fixture compiles");

    assert_eq!(plan.dial_of_call("c1"), Some("s1"), "the INVITE that opens leg A");
    assert_eq!(plan.dial_of_call("c2"), Some("s8"), "the INVITE that opens leg C");
    assert_eq!(plan.dial_of_call("c9"), None, "a call the document does not declare");

    assert_eq!(plan.call_dialled_by("s1"), Some("c1"));
    assert_eq!(plan.call_dialled_by("s8"), Some("c2"));
    // Everything else dials nothing: a callee-side INVITE is witnessed, an ACK
    // and a BYE ride a dialog the dial already opened.
    for step in ["s3", "s6", "s10", "s15"] {
        assert_eq!(plan.call_dialled_by(step), None, "{step} is not a dial");
    }
}

/// Compile `flow` inside a minimal two-leg document and return the refusals.
fn refusals(flow: &str) -> Vec<PlanError> {
    refusals_with(flow, r#""postconditions": { "cdr": { "absent": "unit test" } },"#, "")
}

fn refusals_with(flow: &str, postconditions: &str, deviations: &str) -> Vec<PlanError> {
    let text = format!(
        r#"{{
          "pivot_version": 3,
          "case": {{ "id": "t", "title": "t", "family": "transparent", "variant": "repro",
                    "origin": "authored", "lanes": {{ "upstream-fake": "ok" }} }},
          "identities": [ {{ "name": "caller", "kind": "external-caller", "forms": ["private"] }} ],
          "calls": [ {{ "id": "c1", "caller_leg": "A", "attempts": [
             {{ "branch": 0, "position": 0, "leg": "B", "callee": {{ "identity": "caller" }} }} ] }} ],
          "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }} ],
          "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                      {{ "id": "uas1", "kind": "uas", "endpoint": "ep0" }} ],
          "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                    {{ "id": "B", "actor": "uas1", "dir": "in" }} ],
          "flow": {flow},
          {deviations}
          {postconditions}
          "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
        }}"#
    );
    let document = PivotV3::from_json(&text)
        .unwrap_or_else(|e| panic!("the fixture parses: {e}\n{text}"));
    Plan::compile(document).err().unwrap_or_default()
}

const D: &str = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;

#[test]
fn a_step_that_states_no_discriminator_is_refused_by_name() {
    let flow = format!(r#"[{{"id":"s1","leg":"A","op":"send","msg":{{}},"delay":{D}}}]"#);
    assert_eq!(refusals(&flow), [PlanError::NoDiscriminator { step: "s1".into() }]);
}

#[test]
fn an_expect_that_states_no_check_mode_is_refused_rather_than_defaulted() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"expect","msg":{{"status":200,"cseq-method":"INVITE"}},"delay":{D}}}]"#
    );
    assert_eq!(refusals(&flow), [PlanError::ExpectWithoutCheckMode { step: "s1".into() }]);
}

/// A race is two steps armed together on ONE leg's frontier: across legs there
/// is no order to revoke, and over a gap the skipped steps' order is real.
#[test]
fn an_overlap_holds_only_between_neighbours_on_one_leg() {
    let across = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"BYE"}},"delay":{D}}},
           {{"id":"s2","leg":"B","op":"expect","check":"record","overlap":"s1",
             "msg":{{"method":"BYE"}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&across),
        [PlanError::OverlapCrossesLegs {
            step: "s2".into(),
            overlap: "s1".into(),
            leg: "B".into(),
            other: "A".into()
        }]
    );

    let over_a_gap = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"BYE"}},"delay":{D}}},
           {{"id":"s2","leg":"A","op":"expect","check":"record","msg":{{"status":100,"cseq-method":"INVITE"}},"delay":{D}}},
           {{"id":"s3","leg":"A","op":"expect","check":"record","overlap":"s1",
             "msg":{{"method":"BYE"}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&over_a_gap),
        [PlanError::OverlapNotAdjacent { step: "s3".into(), overlap: "s1".into() }]
    );
}

/// An `overlap` is a backwards reference like every other (§6.2).
#[test]
fn an_overlap_naming_a_step_that_has_not_run_is_refused() {
    let forward = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"BYE"}},"overlap":"s2","delay":{D}}},
           {{"id":"s2","leg":"A","op":"expect","check":"record","msg":{{"method":"BYE"}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&forward),
        [PlanError::ForwardReference {
            site: "step \"s1\" overlap".into(),
            reference: "s2".into()
        }]
    );
}

#[test]
fn a_forward_anchor_and_an_unknown_one_are_told_apart() {
    let forward = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},
             "delay":{{"ms":0,"from":"step:s2","compressible":true,"timer_linked":false}}}},
           {{"id":"s2","leg":"A","op":"expect","check":"record","msg":{{"status":100,"cseq-method":"INVITE"}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&forward),
        [PlanError::ForwardReference {
            site: "step \"s1\" delay anchor".into(),
            reference: "s2".into(),
        }]
    );
    let unknown = r#"[{"id":"s1","leg":"A","op":"send","msg":{"method":"INVITE"},
             "delay":{"ms":0,"from":"step:sX","compressible":true,"timer_linked":false}}]"#.to_string();
    assert_eq!(
        refusals(&unknown),
        [PlanError::UnknownReference {
            site: "step \"s1\" delay anchor".into(),
            reference: "sX".into(),
        }]
    );
}

#[test]
fn a_reference_into_an_alt_branch_from_outside_is_refused() {
    let flow = format!(
        r#"[{{"id":"a1","op":"alt","branches":[
              {{"name":"one","steps":[{{"id":"s1","leg":"A","op":"expect","check":"record","msg":{{"status":200,"cseq-method":"INVITE"}},"delay":{D}}}]}},
              {{"name":"two","steps":[{{"id":"s2","leg":"A","op":"expect","check":"record","msg":{{"status":487,"cseq-method":"INVITE"}},"delay":{D}}}]}}]}},
            {{"id":"s3","leg":"A","op":"send","after":["s1"],"msg":{{"method":"ACK"}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&flow),
        [PlanError::CrossBranchReference {
            site: "step \"s3\" after".into(),
            reference: "s1".into(),
        }]
    );
}

#[test]
fn an_alt_whose_branches_share_a_first_discriminator_is_refused() {
    let flow = format!(
        r#"[{{"id":"a1","op":"alt","branches":[
              {{"name":"one","steps":[{{"id":"s1","leg":"A","op":"expect","check":"record","msg":{{"status":200,"cseq-method":"INVITE"}},"delay":{D}}}]}},
              {{"name":"two","steps":[{{"id":"s2","leg":"A","op":"expect","check":"record","msg":{{"status":200,"cseq-method":"INVITE"}},"delay":{D}}}]}}]}}]"#
    );
    assert_eq!(
        refusals(&flow),
        [PlanError::AltIndiscriminable {
            alt: "a1".into(),
            left: "one".into(),
            right: "two".into(),
            discriminator: "A 200 INVITE".into(),
        }]
    );
}

#[test]
fn an_alt_branch_opening_on_a_send_or_on_a_tolerated_absence_is_refused() {
    let flow = format!(
        r#"[{{"id":"a1","op":"alt","branches":[
              {{"name":"one","steps":[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"ACK"}},"delay":{D}}}]}},
              {{"name":"two","steps":[{{"id":"s2","leg":"A","op":"expect","optional":true,"check":"record","msg":{{"status":487,"cseq-method":"INVITE"}},"delay":{D}}}]}}]}}]"#
    );
    let found = refusals(&flow);
    assert!(
        found.contains(&PlanError::AltBranchOpensOnSend {
            alt: "a1".into(),
            branch: "one".into(),
            step: "s1".into()
        }),
        "{found:?}"
    );
    assert!(
        found.contains(&PlanError::AltBranchOpensOnOptional {
            alt: "a1".into(),
            branch: "two".into(),
            step: "s2".into()
        }),
        "{found:?}"
    );
}

#[test]
fn an_unordered_group_that_holds_a_send_or_one_member_is_refused() {
    let flow = format!(
        r#"[{{"id":"u1","op":"unordered","steps":[
              {{"id":"s1","leg":"A","op":"send","msg":{{"method":"ACK"}},"delay":{D}}}]}}]"#
    );
    let found = refusals(&flow);
    assert!(found.contains(&PlanError::UnorderedTooFewSteps { group: "u1".into(), steps: 1 }));
    assert!(found.contains(&PlanError::UnorderedHoldsASend {
        group: "u1".into(),
        step: "s1".into()
    }));
}

#[test]
fn one_member_of_an_unordered_group_may_not_reference_another() {
    let flow = format!(
        r#"[{{"id":"u1","op":"unordered","steps":[
              {{"id":"s1","leg":"A","op":"expect","check":"record","msg":{{"status":481,"cseq-method":"INVITE"}},"delay":{D}}},
              {{"id":"s2","leg":"A","op":"expect","check":"record","after":["s1"],"msg":{{"method":"BYE"}},"delay":{D}}}]}}]"#
    );
    assert_eq!(
        refusals(&flow),
        [PlanError::UnorderedInternalReference {
            group: "u1".into(),
            from: "s2".into(),
            to: "s1".into(),
        }]
    );
}

#[test]
fn an_accessor_naming_something_the_document_does_not_hold_is_refused_by_site() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"REFER",
             "headers":[{{"name":"Refer-To","value":"<sip:${{num:transferee:e164}}@h>"}}]}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&flow),
        [PlanError::UnknownIdentity {
            site: "step \"s1\" header \"Refer-To\"".into(),
            name: "transferee".into(),
        }]
    );
    let wrong_form = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"REFER",
             "headers":[{{"name":"Refer-To","value":"<sip:${{num:caller:e164}}@h>"}}]}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&wrong_form),
        [PlanError::UndeclaredDialForm {
            site: "step \"s1\" header \"Refer-To\"".into(),
            name: "caller".into(),
            form: "e164".into(),
        }]
    );
}

#[test]
fn a_branch_accessor_on_something_that_is_not_an_alt_is_refused() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{D}}},
            {{"id":"s2","leg":"A","op":"send","msg":{{"method":"INFO",
              "headers":[{{"name":"X-Branch","value":"${{step:s1.branch}}"}}]}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&flow),
        [PlanError::BranchAccessorOnNonAlt {
            site: "step \"s2\" header \"X-Branch\"".into(),
            step: "s1".into(),
        }]
    );
}

#[test]
fn a_deviation_missing_its_payload_or_naming_a_scripted_step_is_refused() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{D}}}]"#
    );
    let found = refusals_with(
        &flow,
        r#""postconditions": { "cdr": { "absent": "unit test" } },"#,
        r#""deviations": [ { "id": "d1", "kind": "cseq-override", "leg": "A" },
                          { "id": "d2", "kind": "suppress-auto", "step": "s1" } ],"#,
    );
    assert!(found.contains(&PlanError::DeviationMissingPayload {
        deviation: "d1".into(),
        kind: "cseq-override".into(),
        missing: "value".into(),
    }), "{found:?}");
    assert!(found.contains(&PlanError::SuppressesScriptedStep {
        deviation: "d2".into(),
        step: "s1".into(),
    }), "{found:?}");
}

/// §11: a `verbatim-emission` on a transaction-derived step COMPILES. The
/// refusal that used to sit here rested on "an automatic has no stored
/// message", which issue 76 made false — such a step stores what any step
/// stores, so there is a block to preserve.
#[test]
fn a_verbatim_emission_naming_an_automatic_compiles() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{D}}},
            {{"id":"s2","leg":"A","op":"send","auto":true,"msg":{{"cseq":1,"method":"ACK",
              "headers":[{{"name":"P-Options","value":"x"}}]}},"delay":{D}}}]"#
    );
    let found = refusals_with(
        &flow,
        r#""postconditions": { "cdr": { "absent": "unit test" } },"#,
        r#""deviations": [ { "id": "d1", "kind": "verbatim-emission", "step": "s2",
                            "preserve": ["header-order"] } ],"#,
    );
    assert_eq!(found, []);
}

#[test]
fn an_unknown_deviation_kind_compiles_so_the_run_can_name_it() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{D}}}]"#
    );
    let found = refusals_with(
        &flow,
        r#""postconditions": { "cdr": { "absent": "unit test" } },"#,
        r#""deviations": [ { "id": "d1", "kind": "drop-every-third-packet", "leg": "A" } ],"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_timer_linked_dwell_that_claims_to_be_compressible_is_refused() {
    let flow = r#"[{"id":"s1","leg":"A","op":"send","msg":{"method":"BYE"},
             "delay":{"ms":65000,"from":"trigger","compressible":true,"timer_linked":true}}]"#.to_string();
    assert_eq!(refusals(&flow), [PlanError::CompressibleTimerLinkedDwell { step: "s1".into() }]);
}

#[test]
fn a_check_whose_op_and_value_disagree_is_refused_at_both_sites() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"expect","check":"assert",
             "checks":[{{"field":"status","op":"eq"}}],
             "msg":{{"status":200,"cseq-method":"INVITE"}},"delay":{D}}}]"#
    );
    let found = refusals_with(
        &flow,
        r#""postconditions": { "cdr": { "absent": "u" },
                              "checks": [ { "field": "m", "op": "exists", "value": "1" } ] },"#,
        "",
    );
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().any(|e| matches!(e, PlanError::CheckValueMismatch { site, .. } if site.starts_with("step"))));
    assert!(found.iter().any(|e| matches!(e, PlanError::CheckValueMismatch { site, .. } if site == "postcondition check")));
}

#[test]
fn every_refusal_is_collected_rather_than_stopping_at_the_first() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"Z","op":"expect","msg":{{}},"delay":{D}}},
            {{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE","status":200}},"delay":{D}}}]"#
    );
    let found = refusals(&flow);
    assert!(found.len() >= 3, "{found:?}");
    assert!(found.iter().any(|e| matches!(e, PlanError::DuplicateId { .. })));
    assert!(found.iter().any(|e| matches!(e, PlanError::NoDiscriminator { .. })));
    assert!(found.iter().any(|e| matches!(e, PlanError::BothMethodAndStatus { .. })));
}

/// §11.1: an `rfc_violations` entry that lands on no step, or names an emitter
/// the document does not declare, is refused by site — a violation nothing can
/// be attributed to gates nothing and points nowhere.
#[test]
fn an_rfc_violation_that_anchors_nowhere_or_names_no_emitter_is_refused() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{D}}}]"#
    );
    // The block rides the slot before `postconditions`, like `deviations` does.
    let violations = |entry: &str| format!(r#""rfc_violations": [{entry}],"#);

    let unknown_step = refusals_with(
        &flow,
        r#""postconditions": { "cdr": { "absent": "unit test" } },"#,
        &violations(r#"{"rule":"no-200-after-cancel","step":"sX","emitter":"uas1"}"#),
    );
    assert_eq!(
        unknown_step,
        [PlanError::UnknownReference {
            site: "rfc violation no-200-after-cancel".into(),
            reference: "sX".into(),
        }]
    );

    let unknown_emitter = refusals_with(
        &flow,
        r#""postconditions": { "cdr": { "absent": "unit test" } },"#,
        &violations(r#"{"rule":"no-200-after-cancel","step":"s1","emitter":"uas9"}"#),
    );
    assert_eq!(
        unknown_emitter,
        [PlanError::UnknownReference {
            site: "rfc violation no-200-after-cancel".into(),
            reference: "uas9".into(),
        }]
    );

    // An actor of the document, and the system under test, both compile.
    for emitter in ["uas1", "sut"] {
        let ok = refusals_with(
            &flow,
            r#""postconditions": { "cdr": { "absent": "unit test" } },"#,
            &violations(&format!(
                r#"{{"rule":"no-200-after-cancel","step":"s1","emitter":"{emitter}"}}"#
            )),
        );
        assert!(ok.is_empty(), "{emitter}: {ok:#?}");
    }
}

/// The rule vocabulary is closed at the SCHEMA, so an unknown token never
/// reaches the compiler at all — which is the difference between a violation
/// rule and a deviation `kind`.
#[test]
fn an_unknown_rfc_violation_rule_does_not_even_parse() {
    let text = r#"{"rule":"answer-after-cancel","step":"s1","emitter":"uas1"}"#;
    assert!(serde_json::from_str::<pivot_schema::violation::RfcViolation>(text).is_err());
}

// ── reliable provisionals and early dialogs (RFC 3262 §3, §6.1) ──────────────

/// §6.1 states one thing about `early`: the dialog a UAS-simulated fork ANSWERS
/// under. A request answers nothing, so a request `send` carrying one is refused
/// rather than read as some other kind of scoping.
#[test]
fn an_early_dialog_on_a_request_send_is_refused_by_name() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","early":"f1","msg":{{"method":"INVITE"}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&flow),
        [PlanError::EarlyDialogOnRequestSend { step: "s1".into(), early: "f1".into() }]
    );
}

/// The OBSERVED reading of §6.1: a response expect naming a fork establishes
/// it — the peer mints the To-tag and the run learns it from the first arrival
/// — so two ids on a caller leg's two provisionals compile.
#[test]
fn an_early_dialog_a_response_expect_observes_compiles() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{D}}},
            {{"id":"s2","leg":"A","op":"expect","check":"record","early":"f1",
              "msg":{{"status":183,"cseq-method":"INVITE"}},"delay":{D}}},
            {{"id":"s3","leg":"A","op":"expect","check":"record","early":"f2",
              "msg":{{"status":180,"cseq-method":"INVITE"}},"delay":{D}}}]"#
    );
    assert_eq!(refusals(&flow), []);
}

/// An early dialog nothing on the leg establishes — no response send answers
/// under it, no response expect observes it — has no To-tag from either side,
/// and gating on one the interpreter invented would assert against itself.
#[test]
fn an_early_dialog_nothing_establishes_is_refused_by_name() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{D}}},
            {{"id":"s2","leg":"A","op":"expect","check":"record","early":"f1",
              "msg":{{"method":"PRACK"}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&flow),
        [PlanError::EarlyDialogUnestablished {
            step: "s2".into(),
            leg: "A".into(),
            early: "f1".into(),
        }]
    );
}

/// One fork is one side's dialog: an id a send answers under AND an expect
/// observes claims two dialogs — the run's own tag and the peer's — under one
/// name, and is refused rather than read as either.
#[test]
fn an_early_dialog_both_answered_and_observed_is_refused_by_name() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"expect","check":"record","msg":{{"method":"INVITE"}},"delay":{D}}},
            {{"id":"s2","leg":"A","op":"send","early":"f1",
              "msg":{{"status":180,"cseq-method":"INVITE"}},"delay":{D}}},
            {{"id":"s3","leg":"A","op":"expect","check":"record","early":"f1",
              "msg":{{"status":183,"cseq-method":"INVITE"}},"delay":{D}}}]"#
    );
    assert_eq!(
        refusals(&flow),
        [PlanError::EarlyDialogAnsweredAndObserved { leg: "A".into(), early: "f1".into() }]
    );
}

/// A fork that DOES answer under the id carries every other step of that dialog
/// with it — the expect above is refused only because nothing answered.
#[test]
fn an_early_dialog_its_leg_answers_under_compiles() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"expect","check":"record","msg":{{"method":"INVITE"}},"delay":{D}}},
            {{"id":"s2","leg":"A","op":"send","early":"f1",
              "msg":{{"status":183,"cseq-method":"INVITE",
                      "headers":[{{"name":"Require","value":"100rel"}},{{"name":"RSeq","value":"1"}}]}},"delay":{D}}},
            {{"id":"s3","leg":"A","op":"expect","check":"record","early":"f1",
              "msg":{{"method":"PRACK"}},"delay":{D}}}]"#
    );
    assert_eq!(refusals(&flow), []);
}

/// RFC 3262 §3 puts an `RSeq` on every reliable provisional, and it is the
/// number the peer's `RAck` quotes. A document that states none states nothing
/// for the interpreter to send, and none is invented.
#[test]
fn a_reliable_provisional_without_an_rseq_is_refused_by_name() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"expect","check":"record","msg":{{"method":"INVITE"}},"delay":{D}}},
            {{"id":"s2","leg":"A","op":"send",
              "msg":{{"status":183,"cseq-method":"INVITE",
                      "headers":[{{"name":"Require","value":"100rel,timer"}}]}},"delay":{D}}}]"#
    );
    assert_eq!(refusals(&flow), [PlanError::ReliableProvisionalWithoutRSeq { step: "s2".into() }]);
}

/// An UNRELIABLE provisional needs no RSeq, and a reliable one that states its
/// own is exactly what the interpreter replays.
#[test]
fn an_unreliable_provisional_needs_no_rseq() {
    let flow = format!(
        r#"[{{"id":"s1","leg":"A","op":"expect","check":"record","msg":{{"method":"INVITE"}},"delay":{D}}},
            {{"id":"s2","leg":"A","op":"send","msg":{{"status":180,"cseq-method":"INVITE"}},"delay":{D}}}]"#
    );
    assert_eq!(refusals(&flow), []);
}
