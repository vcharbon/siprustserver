//! Lint rule fixtures: one broken document per rule, asserted by rule id.
//!
//! Each case starts from a document that lints clean and breaks exactly one
//! thing, so a rule that stops firing is caught, and so is a rule that starts
//! firing on documents it should not. The base is deliberately minimal — an
//! authored two-step call — because a rule proved on a small document is proved
//! everywhere; the shipped fixtures cover breadth.

use pivot_schema::lint::lint_str;
use serde_json::{json, Value};

/// A document that lints clean: one authored call, one send, one expect.
fn base() -> Value {
    json!({
        "pivot_version": 3,
        "case": {
            "id": "base", "title": "one call", "family": "transparent",
            "variant": "repro", "origin": "authored",
            "lanes": { "upstream-fake": "ok" }
        },
        "identities": [
            { "name": "caller", "kind": "external-caller", "observed": "0009001", "forms": ["private"] },
            { "name": "called-0-0", "kind": "site", "observed": "+33000900004", "forms": ["e164"] }
        ],
        "calls": [{
            "id": "c1", "caller_leg": "A",
            "attempts": [{
                "branch": 0, "position": 0, "leg": "B",
                "callee": { "identity": "called-0-0" }
            }]
        }],
        "endpoints": [{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }],
        "actors": [
            { "id": "uac1", "kind": "uac", "endpoint": "ep0" },
            { "id": "uas1", "kind": "uas", "endpoint": "ep0", "claim": { "by": "ruri-pos" } }
        ],
        "legs": [
            { "id": "A", "actor": "uac1", "dir": "out" },
            { "id": "B", "actor": "uas1", "dir": "in" }
        ],
        "flow": [
            {
                "id": "s1", "leg": "A", "op": "send",
                "msg": { "method": "INVITE", "ruri": { "pos": "called[0][0]" } },
                "delay": { "ms": 0, "from": "trigger", "compressible": true, "timer_linked": false }
            },
            {
                "id": "s2", "leg": "B", "op": "expect", "check": "assert",
                "msg": { "method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            }
        ],
        "postconditions": { "cdr": { "count": 1 } },
        "timing": { "expect_budget_ms": 32000, "settle_budget_ms": 32000 }
    })
}

/// Apply `edit` to the base document and lint the result.
fn broken(edit: impl FnOnce(&mut Value)) -> pivot_schema::Report {
    let mut document = base();
    edit(&mut document);
    lint_str(&document.to_string())
}

fn assert_fires(rule: &str, edit: impl FnOnce(&mut Value)) {
    let report = broken(edit);
    assert!(report.rules().contains(rule), "expected {rule}, got:\n{}", report.render());
}

fn assert_clean(rule: &str, edit: impl FnOnce(&mut Value)) {
    let report = broken(edit);
    assert!(!report.rules().contains(rule), "unexpected {rule}:\n{}", report.render());
}

/// A captured document stating a two-detector roster, both outcomes present.
fn rostered(document: &mut Value) {
    document["case"]["origin"] = json!("capture");
    document["case"]["source"] =
        json!({ "capture": "c.pcap.gz", "call_groups": [0], "anonymized": true });
    document["case"]["annotations"] = json!({ "flags": [
        { "kind": "detector-roster", "detail": "refer, mrf" },
        { "kind": "detected-none:refer", "detail": "no REFER crossed this vantage" },
        { "kind": "detected-none:mrf", "detail": "no called leg carries a control dialog" }
    ]});
}

fn roster_flags(document: &mut Value) -> &mut Vec<Value> {
    document["case"]["annotations"]["flags"].as_array_mut().expect("flags")
}

#[test]
fn detector_roster_complete_lints_clean() {
    assert_clean("annotations/detector-roster-incomplete", rostered);
    assert_clean("annotations/detector-outcome-unrostered", rostered);
}

#[test]
fn detector_roster_incomplete_when_a_named_detector_states_nothing() {
    assert_fires("annotations/detector-roster-incomplete", |d| {
        rostered(d);
        roster_flags(d).retain(|f| f["kind"] != "detected-none:mrf");
    });
}

#[test]
fn detector_roster_incomplete_when_a_detector_concludes_twice() {
    assert_fires("annotations/detector-roster-incomplete", |d| {
        rostered(d);
        roster_flags(d).push(json!({ "kind": "detected:mrf", "detail": "a resource joined" }));
    });
}

#[test]
fn detector_outcome_unrostered_when_the_roster_omits_it() {
    assert_fires("annotations/detector-outcome-unrostered", |d| {
        rostered(d);
        roster_flags(d).push(json!({ "kind": "detected-none:prack", "detail": "no PRACK" }));
    });
}

/// An AUTHORED document states no roster and is not held to one: the rules are
/// an account of what an EXTRACTOR read, and nothing extracted an authored case.
#[test]
fn detector_roster_unchecked_on_an_authored_document() {
    assert_clean("annotations/detector-roster-incomplete", |d| {
        rostered(d);
        d["case"]["origin"] = json!("authored");
        d["case"].as_object_mut().expect("case").remove("source");
        roster_flags(d).retain(|f| f["kind"] != "detected-none:mrf");
    });
}

/// The base document restated as a CAPTURE: provenance, span, and a coordinate
/// on every step it already carries.
fn captured_base(document: &mut Value) {
    document["case"]["origin"] = json!("capture");
    document["case"]["source"] =
        json!({ "capture": "c.pcap", "call_groups": [0], "anonymized": true });
    document["timing"]["capture_span_ms"] = json!(120);
    // One coordinate per step: two steps naming one captured message is its own
    // rule, and a fixture that shared one would fire it on every test.
    for (i, step) in flow(document).iter_mut().enumerate() {
        step["observed"] = json!({ "leg": 0, "msg": i, "at_us": i * 10 });
    }
}

fn flow(document: &mut Value) -> &mut Vec<Value> {
    document["flow"].as_array_mut().expect("a flow")
}

/// Answer the base document's call: leg B's dialog-creating 200 (`s3`) and the
/// ACK that confirms it (`s4`). Every step a test appends after this one is
/// in-dialog by §6.1, which is what makes the marker testable on a document
/// whose base has no dialog at all.
fn answered(document: &mut Value) {
    flow(document).push(json!({
        "id": "s3", "leg": "B", "op": "send",
        "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
        "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
    }));
    flow(document).push(json!({
        "id": "s4", "leg": "B", "op": "expect", "check": "record", "auto": true,
        "in_dialog": true, "confirms_dialog": true,
        "msg": { "method": "ACK", "cseq": 1 },
        "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
    }));
}

/// A final on the CALLER's leg that opens no dialog (`s3`). An ACK after it is
/// the §17.1.1.3 ACK the stack composes off that transaction, and it owes no
/// `in_dialog` or `confirms_dialog` marking.
fn rejected(document: &mut Value) {
    flow(document).push(json!({
        "id": "s3", "leg": "A", "op": "expect", "check": "assert",
        "msg": { "status": 480, "reason": "Temporarily Unavailable", "cseq-method": "INVITE" },
        "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
    }));
}

/// A re-INVITE inside the answered dialog, its 200 and the ACK that answers it
/// (`s5`..`s8`). Every one of those steps is in-dialog, and NONE of them
/// confirms anything: the dialog `s3` created was confirmed by `s4`.
fn reinvited(document: &mut Value) {
    answered(document);
    for step in [
        json!({
            "id": "s5", "leg": "B", "op": "expect", "check": "assert", "in_dialog": true,
            "msg": { "method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
        }),
        json!({
            "id": "s6", "leg": "B", "op": "send", "auto": true, "in_dialog": true,
            "msg": { "status": 100, "reason": "Trying", "cseq-method": "INVITE", "cseq": 2 },
            "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false }
        }),
        json!({
            "id": "s7", "leg": "B", "op": "send", "in_dialog": true,
            "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s6", "compressible": true, "timer_linked": false }
        }),
        json!({
            "id": "s8", "leg": "B", "op": "expect", "check": "record", "auto": true,
            "in_dialog": true,
            "msg": { "method": "ACK", "cseq": 2 },
            "delay": { "ms": 0, "from": "step:s7", "compressible": true, "timer_linked": false }
        }),
    ] {
        flow(document).push(step);
    }
}

#[test]
fn the_base_document_lints_clean() {
    let report = lint_str(&base().to_string());
    assert!(!report.has_errors(), "{}", report.render());
    assert!(report.diagnostics.is_empty(), "{}", report.render());
}

#[test]
fn a_malformed_document_reports_the_parse_error_and_nothing_else() {
    let report = lint_str("{\"pivot_version\": 3");
    assert_eq!(report.rules().into_iter().collect::<Vec<_>>(), ["schema/parse"]);
}

#[test]
fn a_version_this_linter_does_not_model_is_refused() {
    assert_fires("schema/version", |d| d["pivot_version"] = json!(2));
}

// --- ids and references -------------------------------------------------

#[test]
fn a_duplicate_or_dotted_id_is_refused() {
    assert_fires("id/duplicate", |d| flow(d)[1]["id"] = json!("s1"));
    assert_fires("id/dot", |d| {
        flow(d)[1]["id"] = json!("s.2");
    });
}

#[test]
fn an_unresolvable_reference_names_what_it_could_not_find() {
    assert_fires("ref/step-leg-unknown", |d| flow(d)[1]["leg"] = json!("Z"));
    assert_fires("ref/actor-endpoint-unknown", |d| d["actors"][0]["endpoint"] = json!("ep9"));
    assert_fires("ref/leg-actor-unknown", |d| d["legs"][0]["actor"] = json!("uac9"));
    assert_fires("ref/call-caller-leg-unknown", |d| d["calls"][0]["caller_leg"] = json!("Z"));
    assert_fires("ref/attempt-leg-unknown", |d| d["calls"][0]["attempts"][0]["leg"] = json!("Z"));
    assert_fires("ref/anchor-unknown", |d| flow(d)[1]["delay"]["from"] = json!("step:s9"));
    assert_fires("ref/after-unknown", |d| flow(d)[1]["after"] = json!(["s9"]));
    assert_fires("ref/defect-step-unknown", |d| {
        d["case"]["defect"] = json!({ "marker": { "step": "s9" }, "description": "x" });
    });
    assert_fires("ref/deviation-step-unknown", |d| {
        d["deviations"] = json!([{ "id": "d1", "kind": "raw-order", "step": "s9" }]);
    });
    assert_fires("ref/callee-identity-unknown", |d| {
        d["calls"][0]["attempts"][0]["callee"]["identity"] = json!("called-9-9");
    });
    assert_fires("ref/actor-identity-unknown", |d| {
        d["actors"][0]["identity"] = json!("nobody");
    });
}

/// The registry is a namespace like any other, and `${num:<name>:<form>}`
/// splits on `:`, so a name carrying one is unnameable.
#[test]
fn an_identity_name_is_unique_and_free_of_the_accessor_separator() {
    assert_fires("id/duplicate", |d| d["identities"][1]["name"] = json!("caller"));
    assert_fires("id/colon", |d| {
        d["identities"][1]["name"] = json!("called:0:0");
        d["calls"][0]["attempts"][0]["callee"]["identity"] = json!("called:0:0");
    });
    // A form is the second half of the same token, and splits the same way.
    assert_fires("id/form-colon", |d| d["identities"][1]["forms"] = json!(["e:164"]));
    assert_fires("id/form-colon", |d| d["identities"][1]["forms"] = json!(["${e164}"]));
}

/// A bare `called-0-0` names one chain, exactly as a bare position token does,
/// so a second call makes it ambiguous. An authored name that positions nothing
/// is untouched.
#[test]
fn a_bare_position_name_is_refused_once_the_document_declares_several_calls() {
    let two_calls = |d: &mut Value| {
        let mut second = d["calls"][0].clone();
        second["id"] = json!("c2");
        d["calls"].as_array_mut().expect("calls").push(second);
    };
    assert_fires("id/identity-unqualified", two_calls);
    let report = broken(|d| {
        two_calls(d);
        d["identities"][0]["name"] = json!("c1-caller");
        d["identities"][1]["name"] = json!("c1-called-0-0");
        d["actors"][0]["identity"] = json!("c1-caller");
        for call in d["calls"].as_array_mut().expect("calls") {
            call["attempts"][0]["callee"]["identity"] = json!("c1-called-0-0");
        }
    });
    assert!(!report.rules().contains("id/identity-unqualified"), "{}", report.render());
}

#[test]
fn a_position_that_names_no_attempt_is_refused() {
    assert_fires("ref/pos-malformed", |d| {
        flow(d)[0]["msg"]["ruri"] = json!({ "pos": "called[0]" })
    });
    assert_fires("ref/pos-unknown", |d| {
        flow(d)[0]["msg"]["ruri"] = json!({ "pos": "called[0][7]" })
    });
}

/// A bare `called[b][s]` names one chain, so a second call makes it ambiguous.
#[test]
fn a_bare_position_is_refused_once_the_document_declares_several_calls() {
    assert_fires("ref/pos-unqualified", |d| {
        let mut second = d["calls"][0].clone();
        second["id"] = json!("c2");
        d["calls"].as_array_mut().expect("calls").push(second);
    });
}

#[test]
fn a_reference_that_points_forward_is_refused() {
    assert_fires("order/anchor-forward", |d| flow(d)[0]["delay"]["from"] = json!("step:s2"));
    assert_fires("order/anchor-self", |d| flow(d)[1]["delay"]["from"] = json!("step:s2"));
    assert_fires("order/after-forward", |d| flow(d)[0]["after"] = json!(["s2"]));
}

/// A declared race in the shape a capture stamps: a `send` and an arrival on
/// leg B (`s5`, `s6`) measured from ONE anchor, the field on the later of them.
fn raced_on_one_anchor(document: &mut Value) {
    answered(document);
    flow(document).push(json!({
        "id": "s5", "leg": "B", "op": "send", "in_dialog": true,
        "msg": { "method": "BYE" },
        "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
    }));
    flow(document).push(json!({
        "id": "s6", "leg": "B", "op": "expect", "check": "assert", "in_dialog": true,
        "msg": { "method": "BYE" }, "overlap": "s5",
        "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
    }));
}

/// The other shape: two ARRIVALS on two anchors, whose independence is the
/// point.
fn raced_on_two_anchors(document: &mut Value) {
    answered(document);
    flow(document).push(json!({
        "id": "s5", "leg": "B", "op": "expect", "check": "assert", "in_dialog": true,
        "msg": { "method": "BYE" },
        "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
    }));
    flow(document).push(json!({
        "id": "s6", "leg": "B", "op": "expect", "check": "assert", "in_dialog": true,
        "msg": { "method": "INFO" }, "overlap": "s5",
        "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
    }));
}

/// Both shapes §6.7a states are races, and the lint takes neither for a defect.
#[test]
fn both_shapes_of_a_declared_race_are_accepted() {
    for shape in [raced_on_one_anchor as fn(&mut Value), raced_on_two_anchors] {
        assert_clean("ref/overlap-forward", shape);
        assert_clean("ref/overlap-no-race", shape);
        assert_clean("ref/overlap-not-adjacent", shape);
    }
}

/// The stamp is read when the step arms, so it names the EARLIER of the pair —
/// §6.7a puts the field on the later one, and a document that stamps backwards
/// compiles nowhere.
#[test]
fn a_race_stamped_on_the_earlier_step_of_the_pair_is_refused() {
    assert_fires("ref/overlap-forward", |d| {
        raced_on_one_anchor(d);
        flow(d)[5].as_object_mut().expect("a step").remove("overlap");
        flow(d)[4]["overlap"] = json!("s6");
    });
}

/// A `send` and an arrival on two anchors is neither shape: both dwells are the
/// document's own, so nothing declares a race between them.
#[test]
fn a_send_racing_an_arrival_measured_from_another_anchor_is_refused() {
    assert_fires("ref/overlap-no-race", |d| {
        raced_on_one_anchor(d);
        flow(d)[5]["delay"]["from"] = json!("step:s2");
    });
}

/// The three the lint already made, on the same pair.
#[test]
fn a_race_names_a_declared_neighbour_on_its_own_leg() {
    assert_fires("ref/overlap-unknown", |d| {
        raced_on_one_anchor(d);
        flow(d)[5]["overlap"] = json!("s9");
    });
    assert_fires("ref/overlap-cross-leg", |d| {
        raced_on_one_anchor(d);
        flow(d)[5]["overlap"] = json!("s1");
    });
    assert_fires("ref/overlap-not-adjacent", |d| {
        raced_on_one_anchor(d);
        flow(d)[5]["overlap"] = json!("s2");
    });
}

// --- calls --------------------------------------------------------------

#[test]
fn an_attempt_chain_states_its_key_and_its_causes() {
    assert_fires("attempt/position-duplicate", |d| {
        let attempt = d["calls"][0]["attempts"][0].clone();
        d["calls"][0]["attempts"].as_array_mut().expect("attempts").push(attempt);
    });
    assert_fires("attempt/cause-missing", |d| {
        let mut second = d["calls"][0]["attempts"][0].clone();
        second["position"] = json!(1);
        d["calls"][0]["attempts"].as_array_mut().expect("attempts").push(second);
    });
    assert_fires("call/no-attempts", |d| d["calls"][0]["attempts"] = json!([]));
}

/// A call the routing decision refused dials nobody and points at the final
/// the caller was answered with.
#[test]
fn a_refused_call_points_at_the_final_it_answered_with() {
    fn refuse(d: &mut Value) {
        d["calls"][0]["attempts"] = json!([]);
        d["calls"][0]["refused"] = json!({ "step": "s2" });
        d["flow"][1] = json!({
            "id": "s2", "leg": "A", "op": "expect", "check": "assert",
            "msg": { "status": 480, "reason": "Temporarily Not Available", "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
        });
    }
    assert_clean("call/no-attempts", refuse);
    assert_fires("ref/refused-step-unknown", |d| {
        refuse(d);
        d["calls"][0]["refused"]["step"] = json!("s9");
    });
    assert_fires("call/refused-step-not-a-final", |d| {
        refuse(d);
        d["calls"][0]["refused"]["step"] = json!("s1");
    });
    assert_fires("call/refused-with-attempts", |d| {
        let chain = d["calls"][0]["attempts"].clone();
        refuse(d);
        d["calls"][0]["attempts"] = chain;
    });
}

/// A call the CALLER abandoned dials nobody and points at the CANCEL it sent.
#[test]
fn an_abandoned_call_points_at_the_cancel_the_caller_sent() {
    fn abandon(d: &mut Value) {
        d["calls"][0]["attempts"] = json!([]);
        d["calls"][0]["abandoned"] = json!({ "step": "s2" });
        d["flow"][1] = json!({
            "id": "s2", "leg": "A", "op": "send",
            "msg": { "method": "CANCEL" },
            "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
        });
    }
    assert_clean("call/no-attempts", abandon);
    assert_clean("call/abandoned-step-not-a-cancel", abandon);
    assert_fires("ref/abandoned-step-unknown", |d| {
        abandon(d);
        d["calls"][0]["abandoned"]["step"] = json!("s9");
    });
    // The INVITE the caller sent is not the CANCEL it left on.
    assert_fires("call/abandoned-step-not-a-cancel", |d| {
        abandon(d);
        d["calls"][0]["abandoned"]["step"] = json!("s1");
    });
    // A CANCEL the caller RECEIVED is somebody else's abandon.
    assert_fires("call/abandoned-step-not-a-cancel", |d| {
        abandon(d);
        d["flow"][1]["op"] = json!("expect");
        d["flow"][1]["check"] = json!("assert");
    });
    assert_fires("call/abandoned-with-attempts", |d| {
        let chain = d["calls"][0]["attempts"].clone();
        abandon(d);
        d["calls"][0]["attempts"] = chain;
    });
}

/// One vantage cannot show both the platform deciding and the caller leaving
/// before it did.
#[test]
fn a_call_stated_both_refused_and_abandoned_is_refused() {
    assert_fires("call/abandoned-with-refusal", |d| {
        d["calls"][0]["attempts"] = json!([]);
        d["calls"][0]["refused"] = json!({ "step": "s3" });
        d["calls"][0]["abandoned"] = json!({ "step": "s2" });
        d["flow"][1] = json!({
            "id": "s2", "leg": "A", "op": "send",
            "msg": { "method": "CANCEL" },
            "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
        });
        d["flow"].as_array_mut().expect("flow").push(json!({
            "id": "s3", "leg": "A", "op": "expect", "check": "assert",
            "msg": { "status": 487, "reason": "Request Terminated", "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
    });
}

#[test]
fn a_no_answer_dwell_outside_its_cause_or_its_band_is_refused() {
    assert_fires("attempt/no-answer-undeclarable", |d| {
        d["calls"][0]["attempts"][0]["no_answer_ms"] = json!(15_139);
    });
    assert_fires("attempt/no-answer-undeclarable", |d| {
        d["calls"][0]["attempts"][0]["cause"] = json!("no-answer");
        d["calls"][0]["attempts"][0]["no_answer_ms"] = json!(92);
    });
}

/// A lane dials attempt `s` of every branch one number, so two `ruri-pos`
/// claims on one endpoint cannot both be reached — and no lane may say `ok`.
#[test]
fn two_claims_that_dial_the_same_number_block_every_lane() {
    assert_fires("claim/same-number-ambiguous", |d| {
        d["actors"].as_array_mut().expect("actors").push(json!(
            { "id": "uas2", "kind": "uas", "endpoint": "ep0", "claim": { "by": "ruri-pos" } }
        ));
        d["legs"]
            .as_array_mut()
            .expect("legs")
            .push(json!({ "id": "C", "actor": "uas2", "dir": "in" }));
        let mut fork = d["calls"][0]["attempts"][0].clone();
        fork["branch"] = json!(1);
        fork["leg"] = json!("C");
        d["calls"][0]["attempts"].as_array_mut().expect("attempts").push(fork);
    });
}

/// A joined leg is a parallel arrival, and the event that joined it belongs to
/// the same call.
#[test]
fn a_joined_leg_points_at_a_join_of_its_own_call() {
    let join = |d: &mut Value, joined: Value| {
        let mut second = d["calls"][0]["attempts"][0].clone();
        second["branch"] = json!(1);
        second["joined_by"] = joined;
        d["calls"][0]["attempts"].as_array_mut().expect("attempts").push(second);
    };
    assert_fires("ref/joined-step-unknown", |d| {
        join(d, json!({ "kind": "mrf", "step": "s9" }));
    });
    assert_fires("attempt/joined-step-other-call", |d| {
        // A second call on its own legs; the join then names a step of neither.
        d["legs"]
            .as_array_mut()
            .expect("legs")
            .push(json!({ "id": "C", "actor": "uac1", "dir": "out" }));
        let mut other = d["calls"][0].clone();
        other["id"] = json!("c2");
        other["caller_leg"] = json!("C");
        other["attempts"][0]["leg"] = json!("C");
        d["calls"].as_array_mut().expect("calls").push(other);
        let step = json!({
            "id": "s3", "leg": "C", "op": "send",
            "msg": { "method": "REFER" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        });
        flow(d).push(step);
        join(d, json!({ "kind": "refer", "step": "s3" }));
    });
}

/// A second callee on its own socket. A lane dials attempt `s` of every branch
/// one number, so a joined branch needs an endpoint of its own to stay claimable.
fn transferee_actor(d: &mut Value) {
    d["endpoints"].as_array_mut().expect("endpoints").push(
        json!({ "id": "ep1", "observed": "127.0.0.1:5070", "side": "peer", "binding": "dedicated" }),
    );
    d["actors"].as_array_mut().expect("actors").push(
        json!({ "id": "uas2", "kind": "uas", "endpoint": "ep1", "claim": { "by": "ruri-pos" } }),
    );
}

/// How a leg ENTERED the call and why the platform LEFT it are independent
/// facts: a transfer target can be busy, and the platform can hunt on from it
/// without ever telling the transferor. Both corners the ruling names are here.
#[test]
fn a_joined_leg_may_also_state_why_the_platform_left_it() {
    // Entry and exit on one attempt: the transfer target answered busy.
    let report = broken(|d| {
        let mut joined = d["calls"][0]["attempts"][0].clone();
        joined["branch"] = json!(1);
        joined["leg"] = json!("C");
        joined["joined_by"] = json!({ "kind": "refer", "step": "s1" });
        joined["cause"] = json!("busy");
        d["calls"][0]["attempts"].as_array_mut().expect("attempts").push(joined);
        transferee_actor(d);
        d["legs"]
            .as_array_mut()
            .expect("legs")
            .push(json!({ "id": "C", "actor": "uas2", "dir": "in" }));
        flow(d).push(json!({
            "id": "s3", "leg": "C", "op": "expect", "check": "assert",
            "msg": { "method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
    });
    assert!(!report.has_errors(), "{}", report.render());

    // …and the platform then reroutes internally, the transferor never notified:
    // the joined attempt is position 0 of its branch, the reroute position 1.
    let report = broken(|d| {
        let mut joined = d["calls"][0]["attempts"][0].clone();
        joined["branch"] = json!(1);
        joined["leg"] = json!("C");
        joined["joined_by"] = json!({ "kind": "refer", "step": "s1" });
        joined["cause"] = json!("busy");
        let mut onward = d["calls"][0]["attempts"][0].clone();
        onward["branch"] = json!(1);
        onward["position"] = json!(1);
        onward["leg"] = json!("D");
        onward["callee"]["identity"] = json!("forward-target");
        let attempts = d["calls"][0]["attempts"].as_array_mut().expect("attempts");
        attempts.push(joined);
        attempts.push(onward);
        d["identities"]
            .as_array_mut()
            .expect("identities")
            .push(json!({ "name": "forward-target", "kind": "site", "forms": ["e164"] }));
        transferee_actor(d);
        let legs = d["legs"].as_array_mut().expect("legs");
        legs.push(json!({ "id": "C", "actor": "uas2", "dir": "in" }));
        legs.push(json!({ "id": "D", "actor": "uas2", "dir": "in" }));
        for (id, leg, from) in [("s3", "C", "step:s2"), ("s4", "D", "step:s3")] {
            flow(d).push(json!({
                "id": id, "leg": leg, "op": "expect", "check": "assert",
                "msg": { "method": "INVITE" },
                "delay": { "ms": 0, "from": from, "compressible": true, "timer_linked": false }
            }));
        }
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// The chain is not conditional, and the join is what ADDED the leg: it runs on
/// every run, and it runs before the leg's own first message.
#[test]
fn a_join_names_a_step_that_runs_unconditionally_and_before_the_leg_it_joined() {
    let join = |d: &mut Value, step: &str| {
        let mut second = d["calls"][0]["attempts"][0].clone();
        second["branch"] = json!(1);
        second["leg"] = json!("C");
        second["joined_by"] = json!({ "kind": "refer", "step": step });
        d["calls"][0]["attempts"].as_array_mut().expect("attempts").push(second);
        d["legs"]
            .as_array_mut()
            .expect("legs")
            .push(json!({ "id": "C", "actor": "uas1", "dir": "in" }));
    };
    // A step inside an `alt` branch happens only on the run that chose it.
    assert_fires("attempt/joined-step-conditional", |d| {
        flow(d).push(json!({ "id": "a1", "op": "alt", "branches": [
            { "name": "answered", "steps": [branch_step("s3", 200)] },
            { "name": "cancelled", "steps": [branch_step("s4", 487)] }
        ] }));
        flow(d).push(json!({
            "id": "s5", "leg": "C", "op": "expect", "check": "assert",
            "msg": { "method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
        join(d, "s3");
    });
    // The joined leg's first message precedes the step that supposedly joined it.
    assert_fires("attempt/joined-step-late", |d| {
        flow(d).push(json!({
            "id": "s3", "leg": "C", "op": "expect", "check": "assert",
            "msg": { "method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
        flow(d).push(json!({
            "id": "s4", "leg": "C", "op": "send",
            "msg": { "status": 200, "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
        }));
        join(d, "s4");
    });
}

#[test]
fn a_document_that_states_no_lane_verdict_is_refused() {
    assert_fires("lanes/unstated", |d| d["case"]["lanes"] = json!({}));
}

// --- steps --------------------------------------------------------------

#[test]
fn check_belongs_to_an_expect_and_to_nothing_else() {
    assert_fires("check/on-send", |d| flow(d)[0]["check"] = json!("assert"));
    assert_fires("check/missing-on-expect", |d| {
        flow(d)[1].as_object_mut().expect("a step").remove("check");
    });
    assert_fires("optional/on-send", |d| flow(d)[0]["optional"] = json!(true));
}

#[test]
fn an_automatic_states_its_transaction_and_records_what_arrives() {
    assert_fires("auto/cseq-missing", |d| flow(d)[1]["auto"] = json!(true));
    assert_fires("auto/not-record", |d| {
        flow(d)[1]["auto"] = json!(true);
        flow(d)[1]["msg"]["cseq"] = json!(1);
    });
    assert_fires("auto/cseq-on-scripted", |d| flow(d)[1]["msg"]["cseq"] = json!(1));
    // §6.3: an automatic stores what any step stores. Frozen headers on one are
    // its content, not a contradiction of the marker.
    assert_clean("auto/not-record", |d| {
        flow(d)[1]["auto"] = json!(true);
        flow(d)[1]["check"] = json!("record");
        flow(d)[1]["msg"]["cseq"] = json!(1);
        flow(d)[1]["msg"]["headers"] = json!([{ "name": "Allow", "value": "INVITE" }]);
    });
}

/// §6.3: three transaction-derived classes carry a body — the ACK to a 2xx
/// (RFC 3261 §13.2.1) and PRACK with its 2xx (RFC 3262 §5). On the two that do
/// not, a stored body would be emitted by nobody.
#[test]
fn a_body_is_refused_where_the_stack_could_not_place_it() {
    let ack_after = |status: u16| {
        move |d: &mut Value| {
            flow(d)[1] = json!({
                "id": "s2", "leg": "A", "op": "expect", "check": "record",
                "msg": { "status": status, "reason": "x", "cseq-method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            });
            flow(d).push(json!({
                "id": "s3", "leg": "A", "op": "send", "auto": true,
                "msg": { "method": "ACK", "cseq": 1, "body": { "ref": "resources/a_0.sdp" } },
                "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
            }));
        }
    };
    assert_clean("auto/body-not-composable", ack_after(200));
    assert_fires("auto/body-not-composable", ack_after(488));
    // Which final an ACK answers is the leg's state, not the nearest final:
    // after a re-INVITE refused 491 (RFC 3261 §14.1), the ACK to the still
    // un-ACKed 2xx carries the delayed offer's answer, and the 491's own ACK
    // carries nothing.
    let after_491_round = |on_2xx_ack: bool| {
        move |d: &mut Value| {
            let body = json!({ "ref": "resources/a_0.sdp" });
            for step in [
                json!({
                    "id": "s3", "leg": "A", "op": "expect", "check": "assert",
                    "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s4", "leg": "A", "op": "send", "in_dialog": true,
                    "msg": { "method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s5", "leg": "A", "op": "expect", "check": "assert", "in_dialog": true,
                    "msg": { "status": 491, "reason": "Request Pending", "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s6", "leg": "A", "op": "send", "auto": true, "in_dialog": true,
                    "msg": { "method": "ACK", "cseq": 2, "body": if on_2xx_ack { Value::Null } else { body.clone() } },
                    "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s7", "leg": "A", "op": "send", "auto": true, "in_dialog": true,
                    "confirms_dialog": true,
                    "msg": { "method": "ACK", "cseq": 1, "body": if on_2xx_ack { body.clone() } else { Value::Null } },
                    "delay": { "ms": 0, "from": "step:s6", "compressible": true, "timer_linked": false }
                }),
            ] {
                let mut step = step;
                if step["msg"]["body"].is_null() {
                    step["msg"].as_object_mut().expect("a msg").remove("body");
                }
                flow(d).push(step);
            }
        }
    };
    assert_clean("auto/body-not-composable", after_491_round(true));
    assert_fires("auto/body-not-composable", after_491_round(false));
    assert_fires("auto/body-not-composable", |d| {
        flow(d)[1] = json!({
            "id": "s2", "leg": "A", "op": "send", "auto": true,
            "msg": { "status": 100, "reason": "Trying", "cseq-method": "INVITE", "cseq": 1,
                     "body": { "ref": "resources/a_0.sdp" } },
            "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
        });
    });
    // An EXPECT asserts what arrived and composes nothing, so its body — a
    // shape or a resource — rides any class.
    for body in [
        json!({ "mode": "absent" }),
        json!({ "ref": "resources/uac1_r0_0.xml", "mode": "frozen", "content-type": "application/example+xml" }),
    ] {
        assert_clean("auto/body-not-composable", |d| {
            flow(d)[1] = json!({
                "id": "s2", "leg": "A", "op": "expect", "check": "record", "auto": true,
                "msg": { "status": 100, "reason": "Trying", "cseq-method": "INVITE", "cseq": 1,
                         "body": body },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            });
        });
    }
}

/// §8.3: `compare` says how an EXPECT holds the received body against its
/// resource. A send emits the resource and compares it with nothing.
#[test]
fn a_body_compare_mode_belongs_to_an_expect_and_to_nothing_else() {
    let info = |op: &str, compare: Option<&str>| {
        let op = op.to_string();
        let compare = compare.map(str::to_string);
        move |d: &mut Value| {
            answered(d);
            let mut step = json!({
                "id": "s5", "leg": "B", "op": op, "in_dialog": true,
                "msg": { "method": "INFO", "body": {
                    "ref": "resources/uas1_r0_0.xml", "mode": "frozen",
                    "content-type": "application/example+xml"
                } },
                "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
            });
            if let Some(compare) = compare {
                step["msg"]["body"]["compare"] = json!(compare);
            }
            if op == "expect" {
                step["check"] = json!("record");
            }
            flow(d).push(step);
        }
    };
    assert_fires("body/compare-on-send", info("send", Some("xml")));
    assert_fires("body/compare-on-send", info("send", Some("exact")));
    assert_clean("body/compare-on-send", info("send", None));
    assert_clean("body/compare-on-send", info("expect", Some("xml")));
    assert_clean("body/compare-on-send", info("expect", None));
}

/// §8.3: `compare: sdp` reads a session description, so it rides a body whose
/// stated content type is `application/sdp` — bare or with parameters — or one
/// stating no type, which is what a bare `application/sdp` resource omits.
#[test]
fn an_sdp_compare_rides_a_session_description_and_nothing_else() {
    let described = |content_type: Option<&str>, compare: &str| {
        let content_type = content_type.map(str::to_string);
        let compare = compare.to_string();
        move |d: &mut Value| {
            answered(d);
            let mut step = json!({
                "id": "s5", "leg": "B", "op": "expect", "check": "record", "in_dialog": true,
                "msg": { "method": "INFO", "body": {
                    "ref": "resources/uas1_r0_0.sdp", "rewrite": ["c=addr", "m=port"],
                    "compare": compare
                } },
                "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
            });
            if let Some(content_type) = content_type {
                step["msg"]["body"]["content-type"] = json!(content_type);
            }
            flow(d).push(step);
        }
    };
    assert_fires("body/compare-sdp-type", described(Some("application/example+xml"), "sdp"));
    assert_clean("body/compare-sdp-type", described(None, "sdp"));
    assert_clean("body/compare-sdp-type", described(Some("application/sdp;charset=utf-8"), "sdp"));
    assert_clean("body/compare-sdp-type", described(Some("Application/SDP"), "sdp"));
    assert_clean("body/compare-sdp-type", described(Some("application/example+xml"), "exact"));
}

/// §6.3: a transaction-derived ACK is composed from the final that answered the
/// INVITE, so a flow that ACKs a transaction it never finalises names a message
/// nothing can build.
#[test]
fn a_transaction_derived_ack_answers_a_final_the_flow_states() {
    let acked = |edit: fn(&mut Value)| {
        move |d: &mut Value| {
            flow(d).push(json!({
                "id": "s3", "leg": "A", "op": "send", "auto": true,
                "msg": { "method": "ACK", "cseq": 1 },
                "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
            }));
            edit(d);
        }
    };
    // Leg A sends the INVITE, nothing answers it, and leg A ACKs anyway.
    assert_fires("auto/ack-without-final", acked(|_| {}));
    // The final on the leg's own steps composes it.
    assert_clean(
        "auto/ack-without-final",
        acked(|d| {
            flow(d)[1] = json!({
                "id": "s2", "leg": "A", "op": "expect", "check": "assert",
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            });
        }),
    );
    // So does one the PEER leg states: a B2BUA relays the final it took, and it
    // reaches leg A on the wire whichever leg's step holds it.
    assert_clean(
        "auto/ack-without-final",
        acked(|d| {
            flow(d)[1] = json!({
                "id": "s2", "leg": "B", "op": "send",
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            });
        }),
    );
    // A SCRIPTED ACK states its own coordinates and needs no transaction.
    assert_clean("auto/ack-without-final", |d| {
        flow(d).push(json!({
            "id": "s3", "leg": "A", "op": "send",
            "msg": { "method": "ACK" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
    });
}

#[test]
fn an_ack_s_count_is_drawn_by_an_automatic_or_it_is_not_stated() {
    assert_fires("retransmits/scripted-ack", |d| {
        flow(d)[0]["msg"] = json!({ "method": "ACK" });
        flow(d)[0]["retransmits"] = json!(2);
    });
    // The same count on the `auto` step that names its transaction is the
    // drawn ladder §6.3 defines, and lints clean.
    let clean = broken(|d| {
        flow(d)[0]["auto"] = json!(true);
        flow(d)[0]["msg"] = json!({ "method": "ACK", "cseq": 1 });
        flow(d)[0]["retransmits"] = json!(2);
    });
    assert!(!clean.rules().contains("retransmits/scripted-ack"), "{}", clean.render());
}

/// Nothing paces a 1xx without `RSeq`, so a count on a SENT one names a ladder
/// no lane can run. On an `expect` the same count is verification of what
/// arrived, whatever drew the copies, and stays legal.
#[test]
fn an_unreliable_provisional_is_not_sent_on_a_ladder() {
    assert_fires("retransmits/unpaced-provisional", |d| {
        flow(d)[0]["msg"] = json!({ "status": 180, "reason": "Ringing", "cseq-method": "INVITE" });
        flow(d)[0]["retransmits"] = json!(1);
    });
    // A RELIABLE provisional rides RFC 3262 §3, and lints clean.
    let reliable = broken(|d| {
        flow(d)[0]["msg"] = json!({
            "status": 180, "reason": "Ringing", "cseq-method": "INVITE",
            "headers": [{ "name": "RSeq", "value": "1" }]
        });
        flow(d)[0]["retransmits"] = json!(1);
    });
    assert!(!reliable.rules().contains("retransmits/unpaced-provisional"), "{}", reliable.render());
    // The count on an `expect` counts arrivals; it asks nobody to emit.
    let counted = broken(|d| {
        flow(d)[1]["msg"] = json!({ "status": 180, "reason": "Ringing", "cseq-method": "INVITE" });
        flow(d)[1]["retransmits"] = json!(1);
    });
    assert!(!counted.rules().contains("retransmits/unpaced-provisional"), "{}", counted.render());
}

#[test]
fn a_message_without_a_discriminator_is_refused() {
    assert_fires("step/discriminator-missing", |d| flow(d)[1]["msg"] = json!({}));
}

/// Compressing a dwell a system timer measures changes what the test proves.
#[test]
fn a_timer_linked_dwell_may_not_be_compressible() {
    assert_fires("delay/timer-linked-compressed", |d| {
        flow(d)[1]["delay"]["timer_linked"] = json!(true)
    });
}

#[test]
fn a_check_states_a_value_exactly_where_its_operator_takes_one() {
    assert_fires("checks/value-mismatch", |d| {
        flow(d)[1]["checks"] = json!([{ "field": "body", "op": "eq" }]);
    });
    assert_fires("checks/value-mismatch", |d| {
        d["postconditions"]["checks"] = json!([{ "field": "m", "op": "absent", "value": "x" }]);
    });
}

// --- alt, unordered, inject ---------------------------------------------

fn with_alt(branches: Value) -> impl FnOnce(&mut Value) {
    move |d: &mut Value| {
        flow(d).push(json!({ "id": "a1", "op": "alt", "branches": branches }));
    }
}

fn branch_step(id: &str, status: u16) -> Value {
    json!({
        "id": id, "leg": "A", "op": "expect", "check": "assert",
        "msg": { "status": status, "cseq-method": "INVITE" },
        "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
    })
}

#[test]
fn an_alt_offers_at_least_two_non_empty_branches() {
    assert_fires(
        "alt/too-few-branches",
        with_alt(json!([{ "name": "only", "steps": [branch_step("s3", 200)] }])),
    );
    assert_fires(
        "alt/empty-branch",
        with_alt(json!([
            { "name": "empty", "steps": [] },
            { "name": "answered", "steps": [branch_step("s3", 200)] }
        ])),
    );
}

/// The interpreter commits on a branch's first message and never backtracks.
#[test]
fn branches_must_be_discriminable_by_their_first_message() {
    assert_fires(
        "alt/shared-discriminator",
        with_alt(json!([
            { "name": "answered", "steps": [branch_step("s3", 200)] },
            { "name": "also-answered", "steps": [branch_step("s4", 200)] }
        ])),
    );
    assert_fires(
        "alt/optional-first",
        with_alt(json!([
            { "name": "answered", "steps": [{
                "id": "s3", "leg": "A", "op": "expect", "check": "assert", "optional": true,
                "msg": { "status": 200, "cseq-method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            }] },
            { "name": "cancelled", "steps": [branch_step("s4", 487)] }
        ])),
    );
    assert_fires(
        "alt/send-first",
        with_alt(json!([
            { "name": "answered", "steps": [{
                "id": "s3", "leg": "A", "op": "send",
                "msg": { "method": "BYE" },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            }] },
            { "name": "cancelled", "steps": [branch_step("s4", 487)] }
        ])),
    );
}

#[test]
fn an_unordered_group_holds_at_least_two_messages_it_waits_for() {
    assert_fires("unordered/too-few", |d| {
        flow(d).push(json!({ "id": "u1", "op": "unordered", "steps": [branch_step("s3", 200)] }));
    });
    assert_fires("unordered/not-expect", |d| {
        flow(d).push(json!({ "id": "u1", "op": "unordered", "steps": [
            branch_step("s3", 200),
            { "id": "s4", "leg": "A", "op": "send", "msg": { "method": "BYE" },
              "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false } }
        ] }));
    });
}

#[test]
fn an_injection_names_an_action() {
    assert_fires("inject/action-empty", |d| {
        flow(d).push(json!({ "id": "i1", "op": "inject", "action": "  " }));
    });
}

// --- accessors ----------------------------------------------------------

fn with_header(value: &str) -> impl FnOnce(&mut Value) + '_ {
    move |d: &mut Value| {
        flow(d)[0]["msg"]["headers"] = json!([{ "name": "Refer-To", "value": value }]);
    }
}

#[test]
fn an_accessor_must_name_something_that_exists() {
    assert_fires("accessor/malformed", with_header("<sip:x@h?X-Dialog=${leg:B.tag}>"));
    assert_fires("accessor/malformed", with_header("<sip:x@h?X-Dialog=${leg:B.call-id"));
    assert_fires("accessor/leg-unknown", with_header("<sip:x@h?X-Dialog=${leg:Z.call-id}>"));
    assert_fires("accessor/step-unknown", with_header("<sip:x@h?X=${step:s9.cseq}>"));
    assert_fires("accessor/malformed", with_header("<sip:${num:called-0-0}@h>"));
    assert_fires("accessor/identity-unknown", with_header("<sip:${num:transferee:e164}@h>"));
}

/// An accessor is looked for in EVERY string, and a check's field selector,
/// a postcondition and a deviation's payload are strings like any other.
#[test]
fn an_accessor_is_resolved_wherever_a_document_carries_a_string() {
    assert_fires("accessor/identity-unknown", |d| {
        d["postconditions"]["checks"] = json!([{ "field": "${num:ghost:e164}", "op": "exists" }]);
    });
    assert_fires("accessor/identity-unknown", |d| {
        d["postconditions"]["cdr"]["checks"] =
            json!([{ "field": "events", "op": "eq", "value": "${num:ghost:e164}" }]);
    });
    assert_fires("accessor/identity-unknown", |d| {
        d["deviations"] =
            json!([{ "id": "d1", "kind": "malformed-header", "header": "${num:ghost:e164}" }]);
    });
    assert_fires("accessor/leg-unknown", |d| {
        d["deviations"] =
            json!([{ "id": "d1", "kind": "raw-order", "preserve": ["${leg:Z.call-id}"] }]);
    });
    assert_fires("accessor/step-unknown", |d| {
        flow(d)[1]["checks"] = json!([{ "field": "header(${step:s9.status})", "op": "exists" }]);
    });
}

/// A lane binds an identity in a form the plan resolved; asking for one the
/// registry does not state is a substitution nothing can perform.
#[test]
fn a_number_accessor_must_ask_for_a_form_the_identity_declares() {
    assert_fires("accessor/num-form-unknown", with_header("<sip:${num:called-0-0:private}@h>"));
    // The form the identity DOES declare passes.
    let report = broken(with_header("<sip:${num:called-0-0:e164}@h>"));
    assert!(!report.has_errors(), "{}", report.render());
}

/// §6.1: an early accessor names a fork some step declares. An id no step
/// carries names no dialog, and one two legs carry names two.
#[test]
fn an_early_accessor_must_name_one_leg_s_declared_fork() {
    // The fork the flow declares passes: `s2` rides `f1` and reads its tag.
    let declared = |d: &mut Value| {
        flow(d)[1]["early"] = json!("f1");
        flow(d)[1]["checks"] =
            json!([{ "field": "to.tag", "op": "eq", "value": "${early:f1.tag}" }]);
    };
    let report = broken(declared);
    assert!(!report.has_errors(), "{}", report.render());

    assert_fires("accessor/early-unknown", |d| {
        flow(d)[1]["early"] = json!("f1");
        flow(d)[1]["checks"] =
            json!([{ "field": "to.tag", "op": "eq", "value": "${early:f2.tag}" }]);
    });
    assert_fires("accessor/early-ambiguous", |d| {
        flow(d)[0]["early"] = json!("f1");
        flow(d)[1]["early"] = json!("f1");
        flow(d)[1]["checks"] =
            json!([{ "field": "header(RAck)", "op": "regex", "value": "^${early:f1.rseq} " }]);
    });
    assert_fires("accessor/malformed", with_header("<sip:x@h?X=${early:f1.local-tag}>"));
}

/// A value the run has not produced yet is not a value.
#[test]
fn an_accessor_that_reads_a_later_step_is_refused() {
    assert_fires("accessor/forward-step", with_header("<sip:x@h?X=${step:s2.cseq}>"));
}

#[test]
fn only_an_alt_reports_which_branch_ran() {
    assert_fires("accessor/branch-on-non-alt", |d| {
        flow(d)[1]["checks"] = json!([{ "field": "x", "op": "eq", "value": "${step:s1.branch}" }]);
    });
}

// --- postconditions and the generator subset ----------------------------

#[test]
fn a_missing_cdr_expectation_needs_a_reason() {
    assert_fires("postconditions/cdr-absent-needs-reason", |d| {
        d.as_object_mut().expect("a document").remove("postconditions");
    });
    assert_fires("postconditions/cdr-absent-needs-reason", |d| {
        d["postconditions"] = json!({ "checks": [{ "field": "m", "op": "exists" }] });
    });
    // Stated as an absence with a reason: accepted.
    let report =
        broken(|d| d["postconditions"] = json!({ "cdr": { "absent": "capture-carries-no-cdr" } }));
    assert!(
        !report.rules().contains("postconditions/cdr-absent-needs-reason"),
        "{}",
        report.render()
    );
}

/// The ONE tolerated absence a capture justifies: a caller-facing PROVISIONAL
/// beyond the peer emissions that anchor it, on a document that NAMES the pass
/// that derived it. Both halves gate — the flag alone would exempt every
/// `optional` in the file, the shape alone would exempt a silent inference.
#[test]
fn a_captured_document_may_tolerate_a_surplus_provisional_it_declares() {
    let ringing = |d: &mut Value| {
        captured_base(d);
        flow(d).push(json!({
            "id": "s3", "leg": "A", "op": "expect", "check": "assert",
            "msg": { "status": 180, "reason": "Ringing", "cseq-method": "INVITE" },
            "optional": true,
            "observed": { "leg": 0, "msg": 0, "at_us": 0 },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
    };
    let declares = |d: &mut Value| {
        d["case"]["annotations"] = json!({ "flags": [{
            "kind": "provisional-expect-surplus-tolerated",
            "detail": "s3 (leg A, 180, 1 of a 2-step run)"
        }]});
    };
    assert_clean("subset/optional", |d| {
        ringing(d);
        declares(d);
    });
    // Declared, but the step is not a provisional: the exemption does not travel.
    assert_fires("subset/optional", |d| {
        ringing(d);
        declares(d);
        flow(d)[2]["msg"] = json!({ "status": 200, "reason": "OK", "cseq-method": "INVITE" });
    });
    // The right shape, silently: an inference the page does not carry.
    assert_fires("subset/optional", ringing);
}

/// Two steps naming one captured message is what a DERIVED provisional
/// expectation does: the SUT emits that message twice, so it is what both
/// datagrams are compared against (§6.9, issue 116). Both halves gate, as they
/// do for `optional` — the flag alone would exempt every duplicate in the file,
/// the shape alone a silent inference.
#[test]
fn a_captured_document_may_derive_a_second_step_on_one_coordinate() {
    let twice = |d: &mut Value| {
        captured_base(d);
        flow(d).push(json!({
            "id": "s3", "leg": "A", "op": "expect", "check": "assert",
            "msg": { "status": 180, "reason": "Ringing", "cseq-method": "INVITE" },
            "observed": { "leg": 0, "msg": 0, "at_us": 0 },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
    };
    let declares = |d: &mut Value| {
        d["case"]["annotations"] = json!({ "flags": [{
            "kind": "relayed-provisional-expect-derived",
            "detail": "s3 (leg A, 180, relays s2, copies s1)"
        }]});
    };
    assert_clean("capture/observed-duplicated", |d| {
        twice(d);
        declares(d);
    });
    // Declared, but the step is not a provisional expect: the exemption does not travel.
    assert_fires("capture/observed-duplicated", |d| {
        twice(d);
        declares(d);
        flow(d)[2]["msg"] = json!({ "status": 200, "reason": "OK", "cseq-method": "INVITE" });
    });
    // The right shape, silently: an inference the page does not carry.
    assert_fires("capture/observed-duplicated", twice);
}

/// The far side of a relayed in-dialog INVITE exchange the capture holds on one
/// leg only (§6.9): the callee leg's record ends at its 2xx, and the caller's
/// re-INVITE, its 2xx and its ACK are transcribed onto it, each naming the
/// caller-leg message it copies. Both halves gate here too: the flag, and the
/// three shapes the pass derives and no other.
#[test]
fn a_captured_document_may_transcribe_a_relayed_re_invite_onto_the_leg_the_vantage_lost() {
    let far_side = |d: &mut Value| {
        captured_base(d);
        unacked(d);
        flow(d)[2]["observed"] = json!({ "leg": 1, "msg": 2, "at_us": 990_000 });
        for step in [
            json!({
                "id": "s4", "leg": "A", "op": "expect", "check": "assert",
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "observed": { "leg": 0, "msg": 2, "at_us": 1_000_000 },
                "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s5", "leg": "A", "op": "send", "auto": true, "in_dialog": true,
                "confirms_dialog": true,
                "msg": { "method": "ACK", "cseq": 1 },
                "observed": { "leg": 0, "msg": 3, "at_us": 1_005_000 },
                "delay": { "ms": 5, "from": "step:s4", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s6", "leg": "A", "op": "send", "in_dialog": true,
                "msg": { "method": "INVITE" },
                "observed": { "leg": 0, "msg": 4, "at_us": 5_000_000 },
                "delay": { "ms": 3995, "from": "step:s5", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s7", "leg": "B", "op": "expect", "check": "record", "in_dialog": true,
                "msg": { "method": "INVITE" },
                "observed": { "leg": 0, "msg": 4, "at_us": 5_000_000 },
                "delay": { "ms": 0, "from": "step:s6", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s8", "leg": "B", "op": "send", "in_dialog": true,
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "observed": { "leg": 0, "msg": 6, "at_us": 5_090_000 },
                "delay": { "ms": 80, "from": "step:s7", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s9", "leg": "A", "op": "expect", "check": "assert", "in_dialog": true,
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "observed": { "leg": 0, "msg": 6, "at_us": 5_090_000 },
                "delay": { "ms": 0, "from": "step:s8", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s10", "leg": "A", "op": "send", "auto": true, "in_dialog": true,
                "msg": { "method": "ACK", "cseq": 2 },
                "observed": { "leg": 0, "msg": 7, "at_us": 5_095_000 },
                "delay": { "ms": 0, "from": "step:s9", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s11", "leg": "B", "op": "expect", "check": "record", "auto": true,
                "in_dialog": true,
                "msg": { "method": "ACK", "cseq": 2 },
                "observed": { "leg": 0, "msg": 7, "at_us": 5_095_000 },
                "delay": { "ms": 0, "from": "step:s10", "compressible": true, "timer_linked": false }
            }),
        ] {
            flow(d).push(step);
        }
    };
    let declares = |d: &mut Value| {
        d["case"]["annotations"] = json!({ "flags": [{
            "kind": "far-side-reinvite-derived",
            "detail": "leg B (record ends at s3): s7 expect INVITE mirrors s6, s8 send 200 mirrors s9, s11 expect ACK mirrors s10"
        }]});
    };
    let report = broken(|d| {
        far_side(d);
        declares(d);
    });
    assert!(!report.has_errors(), "{}", report.render());
    // Declared, but a shape the pass never derives: the exemption does not travel.
    assert_fires("capture/observed-duplicated", |d| {
        far_side(d);
        declares(d);
        flow(d)[6]["msg"] = json!({ "method": "INFO" });
    });
    // Declared and the right shape, but on the near leg itself: the pass pairs
    // one message across the two legs, never twice on one, so a cut that
    // duplicated a step is still the defect the rule is for.
    assert_fires("capture/observed-duplicated", |d| {
        far_side(d);
        declares(d);
        flow(d)[6]["leg"] = json!("A");
    });
    // The right shapes, silently: an inference the page does not carry.
    assert_fires("capture/observed-duplicated", far_side);
}

/// The same pass in the other direction (§6.9): the far party's re-INVITE the
/// platform relayed onto the caller, whose callee leg the vantage lost past its
/// 2xx. The callee's `send INVITE`, `expect 2xx` and auto `send ACK` each name
/// the caller-leg message they mirror, and the rule reads the pair's legs and
/// ops, so the three shapes gate whichever leg emits.
#[test]
fn a_captured_document_may_transcribe_a_relayed_re_invite_the_far_party_sent() {
    let mirror = |d: &mut Value| {
        captured_base(d);
        unacked(d);
        flow(d)[2]["observed"] = json!({ "leg": 1, "msg": 2, "at_us": 990_000 });
        for step in [
            json!({
                "id": "s4", "leg": "A", "op": "expect", "check": "assert",
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "observed": { "leg": 0, "msg": 2, "at_us": 1_000_000 },
                "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s5", "leg": "A", "op": "send", "auto": true, "in_dialog": true,
                "confirms_dialog": true,
                "msg": { "method": "ACK", "cseq": 1 },
                "observed": { "leg": 0, "msg": 3, "at_us": 1_005_000 },
                "delay": { "ms": 5, "from": "step:s4", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s6", "leg": "B", "op": "send", "in_dialog": true,
                "msg": { "method": "INVITE" },
                "observed": { "leg": 0, "msg": 4, "at_us": 5_000_000 },
                "delay": { "ms": 4010, "from": "step:s3", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s7", "leg": "A", "op": "expect", "check": "assert", "in_dialog": true,
                "msg": { "method": "INVITE" },
                "observed": { "leg": 0, "msg": 4, "at_us": 5_000_000 },
                "delay": { "ms": 0, "from": "step:s6", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s8", "leg": "A", "op": "send", "in_dialog": true,
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "observed": { "leg": 0, "msg": 5, "at_us": 5_030_000 },
                "delay": { "ms": 30, "from": "step:s7", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s9", "leg": "B", "op": "expect", "check": "record", "in_dialog": true,
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "observed": { "leg": 0, "msg": 5, "at_us": 5_030_000 },
                "delay": { "ms": 0, "from": "step:s8", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s10", "leg": "B", "op": "send", "auto": true, "in_dialog": true,
                "msg": { "method": "ACK", "cseq": 41 },
                "observed": { "leg": 0, "msg": 6, "at_us": 5_060_000 },
                "delay": { "ms": 10, "from": "step:s9", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s11", "leg": "A", "op": "expect", "check": "record", "auto": true,
                "in_dialog": true,
                "msg": { "method": "ACK", "cseq": 41 },
                "observed": { "leg": 0, "msg": 6, "at_us": 5_060_000 },
                "delay": { "ms": 0, "from": "step:s10", "compressible": true, "timer_linked": false }
            }),
        ] {
            flow(d).push(step);
        }
    };
    let declares = |d: &mut Value| {
        d["case"]["annotations"] = json!({ "flags": [{
            "kind": "far-side-reinvite-derived",
            "detail": "leg B (record ends at s3): s6 send INVITE mirrors s7, s9 expect 200 mirrors s8, s10 send ACK mirrors s11"
        }]});
    };
    let report = broken(|d| {
        mirror(d);
        declares(d);
    });
    assert!(!report.has_errors(), "{}", report.render());
    // A scripted ACK is not the shape: the pass derives the automatic one.
    assert_fires("capture/observed-duplicated", |d| {
        mirror(d);
        declares(d);
        flow(d)[9]["auto"] = json!(false);
        flow(d)[9]["msg"] = json!({ "method": "ACK" });
    });
    // The right shapes, silently: an inference the page does not carry.
    assert_fires("capture/observed-duplicated", mirror);
}

/// A capture shows what DID happen once. It never shows that an absence was
/// tolerable or that a value should be read from a dialog at run time.
#[test]
fn a_captured_document_may_not_carry_an_authored_construct() {
    let captured = captured_base;
    assert_fires("subset/optional", |d| {
        captured(d);
        flow(d)[1]["optional"] = json!(true);
    });
    assert_fires("subset/checks", |d| {
        captured(d);
        flow(d)[1]["checks"] = json!([{ "field": "body", "op": "exists" }]);
    });
    assert_fires("subset/after", |d| {
        captured(d);
        flow(d)[1]["after"] = json!(["s1"]);
    });
    // The OPTIONS-audit shape (OPTIONS, 200, no count) is the one background
    // policy a generated document states — the replaying SUT's own in-dialog
    // keepalive; anything beyond it stays authored-only.
    let report = broken(|d| {
        captured(d);
        d["actors"][1]["background"] =
            json!([{ "match": { "method": "OPTIONS" }, "respond": { "status": 200 } }]);
    });
    assert!(!report.rules().contains("subset/background"), "{}", report.render());
    assert_fires("subset/background", |d| {
        captured(d);
        d["actors"][1]["background"] = json!([{ "match": { "method": "OPTIONS" }, "respond": { "status": 200 }, "count": { "at_least": 1 } }]);
    });
    assert_fires("subset/background", |d| {
        captured(d);
        d["actors"][1]["background"] =
            json!([{ "match": { "method": "INFO" }, "respond": { "status": 200 } }]);
    });
    assert_fires("subset/alt", |d| {
        captured(d);
        flow(d).push(json!({ "id": "a1", "op": "alt", "branches": [
            { "name": "answered", "steps": [branch_step("s3", 200)] },
            { "name": "cancelled", "steps": [branch_step("s4", 487)] }
        ] }));
    });
    assert_fires("subset/unordered", |d| {
        captured(d);
        flow(d).push(json!({ "id": "u1", "op": "unordered", "steps": [branch_step("s3", 200), branch_step("s4", 487)] }));
    });
    assert_fires("subset/inject", |d| {
        captured(d);
        flow(d).push(json!({ "id": "i1", "op": "inject", "action": "node-kill" }));
    });
    assert_fires("subset/accessor", |d| {
        captured(d);
        flow(d)[0]["msg"]["headers"] = json!([{ "name": "X-A", "value": "${leg:A.call-id}" }]);
    });
    // A malformed accessor is refused like a run-time one: a truncated `${`
    // cannot pass as literal text.
    assert_fires("subset/accessor", |d| {
        captured(d);
        flow(d)[0]["msg"]["headers"] = json!([{ "name": "X-A", "value": "${num:called-0-0" }]);
    });
    // A `num:` composition is the one accessor a capture justifies (§6.4): it
    // resolves statically through the document's declared identities and dial
    // forms, wherever it is written.
    for edit in [
        (|d: &mut Value| {
            flow(d)[0]["msg"]["headers"] =
                json!([{ "name": "Refer-To", "value": "<sip:${num:called-0-0:e164}@h>" }]);
        }) as fn(&mut Value),
        |d| {
            d["deviations"] = json!([
                { "id": "d1", "kind": "malformed-header", "step": "s1", "header": "${num:called-0-0:e164}" }
            ]);
        },
    ] {
        let report = broken(|d| {
            captured(d);
            edit(d);
        });
        assert!(!report.rules().contains("subset/accessor"), "{}", report.render());
    }
    assert_fires("subset/postcondition-checks", |d| {
        captured(d);
        d["postconditions"]["checks"] = json!([{ "field": "m", "op": "exists" }]);
    });
    // Correlation cuts a case per call: a capture yields one document per call.
    assert_fires("subset/several-calls", |d| {
        captured(d);
        let mut second = d["calls"][0].clone();
        second["id"] = json!("c2");
        d["calls"].as_array_mut().expect("calls").push(second);
    });
}

/// The gate runs the other way too: a capture must carry what a capture DOES
/// justify.
#[test]
fn a_captured_document_must_carry_its_provenance_and_coordinates() {
    assert_fires("capture/source-missing", |d| d["case"]["origin"] = json!("capture"));
    assert_fires("capture/span-missing", |d| d["case"]["origin"] = json!("capture"));
    assert_fires("capture/observed-missing", |d| d["case"]["origin"] = json!("capture"));
}

#[test]
fn an_authored_document_carrying_a_capture_coordinate_is_flagged_not_refused() {
    let report = broken(|d| flow(d)[0]["observed"] = json!({ "leg": 0, "msg": 0, "at_us": 0 }));
    assert!(report.rules().contains("authored/observed-present"), "{}", report.render());
    assert!(!report.has_errors(), "{}", report.render());
}

// --- regressions: each of these documents once linted clean -------------
//
// One test per demonstrated false negative. They are grouped here rather than
// folded into the rule tests above because what they pin is not "the rule
// exists" but "the rule reaches THIS shape" — the gap between the two is
// where every one of them lived.

/// A response expectation that states no `cseq-method` matches that status on
/// every transaction, so it overlaps one that names a method. Keying the two
/// as distinct strings made an undecidable `alt` look decidable.
#[test]
fn a_branch_without_a_cseq_method_overlaps_one_that_names_it() {
    assert_fires(
        "alt/shared-discriminator",
        with_alt(json!([
            { "name": "any200", "steps": [{
                "id": "s3", "leg": "A", "op": "expect", "check": "assert",
                "msg": { "status": 200 },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            }] },
            { "name": "inv200", "steps": [branch_step("s4", 200)] }
        ])),
    );
    // The wildcard cuts both ways: order does not rescue it.
    assert_fires(
        "alt/shared-discriminator",
        with_alt(json!([
            { "name": "inv200", "steps": [branch_step("s3", 200)] },
            { "name": "any200", "steps": [{
                "id": "s4", "leg": "A", "op": "expect", "check": "assert",
                "msg": { "status": 200 },
                "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
            }] }
        ])),
    );
    // Different statuses on one leg, and different legs, still discriminate.
    let report = broken(with_alt(json!([
        { "name": "answered", "steps": [branch_step("s3", 200)] },
        { "name": "cancelled", "steps": [branch_step("s4", 487)] }
    ])));
    assert!(!report.rules().contains("alt/shared-discriminator"), "{}", report.render());
}

/// The subset gate once scanned header values alone, so an accessor in a frozen
/// tier-2 ref reached a captured document untouched.
#[test]
fn an_accessor_is_found_in_every_string_a_message_carries() {
    /// Where an accessor hides, and the edit that hides it there.
    type Position = (&'static str, fn(&mut Value));
    let positions: [Position; 5] = [
        ("msg.ruri.frozen", |d| {
            flow(d)[0]["msg"]["ruri"] = json!({ "frozen": "${leg:B.call-id}" })
        }),
        ("msg.headers[].name", |d| {
            flow(d)[0]["msg"]["headers"] = json!([{ "name": "${leg:B.call-id}", "value": "x" }]);
        }),
        ("msg.headers-present[]", |d| {
            flow(d)[1]["msg"]["headers-present"] = json!(["${leg:B.call-id}"]);
        }),
        ("msg.body.ref", |d| {
            flow(d)[0]["msg"]["body"] = json!({ "ref": "resources/${leg:B.call-id}.sdp" });
        }),
        ("msg.reason", |d| {
            flow(d)[1]["msg"] = json!({ "status": 200, "reason": "${leg:B.call-id}" });
        }),
    ];
    for (where_, edit) in positions {
        // On a capture: refused outright, wherever it hides.
        let report = broken(|d| {
            d["case"]["origin"] = json!("capture");
            d["case"]["source"] =
                json!({ "capture": "c.pcap", "call_groups": [0], "anonymized": true });
            d["timing"]["capture_span_ms"] = json!(120);
            for step in flow(d) {
                step["observed"] = json!({ "leg": 0, "msg": 0, "at_us": 0 });
            }
            edit(d);
        });
        assert!(report.rules().contains("subset/accessor"), "{where_}:\n{}", report.render());
        assert!(
            report.diagnostics.iter().any(|diagnostic| diagnostic.path.ends_with(where_)),
            "{where_}: no diagnostic located it\n{}",
            report.render()
        );
    }
}

/// The same positions are RESOLVED on an authored document, not merely
/// tolerated: an unknown leg is an unknown leg wherever it is written.
#[test]
fn an_accessor_outside_a_header_value_is_still_resolved() {
    assert_fires("accessor/leg-unknown", |d| {
        flow(d)[0]["msg"]["ruri"] = json!({ "frozen": "${leg:Z.call-id}" });
    });
    assert_fires("accessor/forward-step", |d| {
        flow(d)[0]["msg"]["ruri"] = json!({ "frozen": "${step:s2.cseq}" });
    });
    assert_fires("accessor/malformed", |d| {
        flow(d)[0]["msg"]["body"] = json!({ "ref": "resources/${leg:B.tag}.sdp" });
    });
}

/// Which branch ran is not known until the alt has completed, so `.branch` is
/// subject to the same points-backwards rule as every other step accessor.
#[test]
fn a_branch_accessor_read_before_its_alt_has_run_is_refused() {
    assert_fires("accessor/forward-step", |d| {
        flow(d)[0]["msg"]["headers"] = json!([{ "name": "X-Ran", "value": "${step:a1.branch}" }]);
        flow(d).push(json!({ "id": "a1", "op": "alt", "branches": [
            { "name": "ok", "steps": [branch_step("s3", 200)] },
            { "name": "cancelled", "steps": [branch_step("s4", 487)] }
        ] }));
    });
    // After the alt, the same accessor is exactly what the grammar is for.
    let report = broken(|d| {
        flow(d).push(json!({ "id": "a1", "op": "alt", "branches": [
            { "name": "ok", "steps": [branch_step("s3", 200)] },
            { "name": "cancelled", "steps": [branch_step("s4", 487)] }
        ] }));
        flow(d).push(json!({
            "id": "s5", "leg": "A", "op": "send",
            "msg": { "method": "BYE", "headers": [{ "name": "X-Ran", "value": "${step:a1.branch}" }] },
            "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
        }));
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// A step inside an `alt` branch runs only when that branch is chosen. Flattening
/// branches into one index made a reference across two of them look backwards.
#[test]
fn a_reference_into_an_alt_branch_from_outside_it_is_refused() {
    let with_two_branches = |d: &mut Value| {
        flow(d).push(json!({ "id": "a1", "op": "alt", "branches": [
            { "name": "ok", "steps": [branch_step("s3", 200)] },
            { "name": "cancelled", "steps": [branch_step("s4", 487)] }
        ] }));
    };
    // A sibling branch's step is not a thing this branch can anchor on.
    assert_fires("order/cross-branch", |d| {
        with_two_branches(d);
        flow(d)[2]["branches"][1]["steps"][0]["delay"]["from"] = json!("step:s3");
    });
    // Nor can a step after the alt wait for one branch's step.
    assert_fires("order/cross-branch", |d| {
        with_two_branches(d);
        flow(d).push(json!({
            "id": "s5", "leg": "A", "op": "send", "after": ["s3"],
            "msg": { "method": "BYE" },
            "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
        }));
    });
    // Nor can a postcondition read one.
    assert_fires("accessor/cross-branch", |d| {
        with_two_branches(d);
        d["postconditions"]["checks"] =
            json!([{ "field": "m", "op": "eq", "value": "${step:s3.status}" }]);
    });
    // Waiting for the ALT is how it is written, and that is accepted.
    let report = broken(|d| {
        with_two_branches(d);
        flow(d).push(json!({
            "id": "s5", "leg": "A", "op": "send", "after": ["a1"],
            "msg": { "method": "BYE" },
            "delay": { "ms": 0, "from": "step:s1", "compressible": true, "timer_linked": false }
        }));
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// Within one branch, ordering works normally — the rule scopes references, it
/// does not forbid them.
#[test]
fn a_later_step_of_the_same_branch_may_reference_an_earlier_one() {
    let report = broken(|d| {
        flow(d).push(json!({ "id": "a1", "op": "alt", "branches": [
            { "name": "ok", "steps": [branch_step("s3", 200), {
                "id": "s6", "leg": "A", "op": "send", "after": ["s3"], "in_dialog": true,
                "confirms_dialog": true,
                "msg": { "method": "ACK" },
                "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
            }] },
            { "name": "cancelled", "steps": [branch_step("s4", 487)] }
        ] }));
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// An order-free group has no internal order, so one member cannot be "earlier"
/// than another — but the whole group has run once a later step reaches it.
#[test]
fn an_unordered_group_has_no_internal_order_to_reference_across() {
    let group = |anchor: &str| {
        let anchor = anchor.to_string();
        move |d: &mut Value| {
            flow(d).push(json!({ "id": "u1", "op": "unordered", "steps": [
                branch_step("s3", 200),
                {
                    "id": "s4", "leg": "A", "op": "expect", "check": "assert",
                    "msg": { "status": 487, "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": anchor, "compressible": true, "timer_linked": false }
                }
            ] }));
        }
    };
    assert_fires("order/anchor-forward", group("step:s3"));
    let report = broken(group("step:s1"));
    assert!(!report.has_errors(), "{}", report.render());
}

/// Every other object in the document refuses a duplicate key; a flow node
/// buffered through `serde_json::Value` silently took the last one.
#[test]
fn a_duplicate_key_in_a_flow_node_is_refused_like_anywhere_else() {
    let base = base().to_string();
    for (what, broken_text) in [
        ("op", base.replace(r#""op":"send""#, r#""op":"expect","op":"send""#)),
        ("leg", base.replace(r#""leg":"A""#, r#""leg":"Z","leg":"A""#)),
        // Nested one level down, inside the step's own `msg`.
        ("method", base.replace(r#""method":"INVITE""#, r#""method":"BYE","method":"INVITE""#)),
    ] {
        let report = lint_str(&broken_text);
        assert!(report.rules().contains("schema/parse"), "duplicate {what}:\n{}", report.render());
        let error = report.diagnostics[0].message.clone();
        assert!(error.contains(what), "duplicate {what} not named: {error}");
    }
}

/// A deviation's payload has to match the violation its `kind` names.
#[test]
fn a_defined_deviation_kind_must_carry_its_payload() {
    assert_fires("deviation/cseq-override-no-value", |d| {
        d["deviations"] =
            json!([{ "id": "d1", "kind": "cseq-override", "leg": "A", "step": "s1" }]);
    });
    assert_fires("deviation/suppress-auto-no-step", |d| {
        d["deviations"] = json!([{ "id": "d1", "kind": "suppress-auto", "leg": "A" }]);
    });
    // s2 is a scripted expect: the stack never composed it, so nothing is withheld.
    assert_fires("deviation/suppress-auto-not-auto", |d| {
        d["deviations"] =
            json!([{ "id": "d1", "kind": "suppress-auto", "leg": "B", "step": "s2" }]);
    });
    // Pointed at a real automatic, it is the documented way to withhold one.
    let report = broken(|d| {
        rejected(d);
        flow(d).push(json!({
            "id": "s4", "leg": "A", "op": "send", "auto": true,
            "msg": { "method": "ACK", "cseq": 1 },
            "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
        }));
        d["deviations"] =
            json!([{ "id": "d1", "kind": "suppress-auto", "leg": "A", "step": "s4" }]);
    });
    assert!(!report.has_errors(), "{}", report.render());
    // An open kind this crate does not define is not second-guessed.
    let report = broken(|d| {
        d["deviations"] = json!([{ "id": "d1", "kind": "vendor-quirk", "leg": "A" }]);
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// §11: a `verbatim-emission` rides the block the document STORES, and an auto
/// step stores one like any other (§6.3) — so the pairing compiles. The refusal
/// that used to sit here rested on "an automatic stores nothing".
#[test]
fn a_verbatim_emission_naming_an_automatic_is_accepted() {
    let report = broken(|d| {
        rejected(d);
        flow(d).push(json!({
            "id": "s4", "leg": "A", "op": "send", "auto": true,
            "msg": { "method": "ACK", "cseq": 1, "headers": [{ "name": "P-Options", "value": "x" }] },
            "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
        }));
        d["deviations"] = json!([{
            "id": "d1", "kind": "verbatim-emission", "leg": "A", "step": "s4",
            "preserve": ["header-order", "casing"]
        }]);
    });
    assert!(!report.has_errors(), "{}", report.render());
    // The same entry on the scripted send it belongs to is the documented use.
    let report = broken(|d| {
        d["deviations"] = json!([{
            "id": "d1", "kind": "verbatim-emission", "leg": "A", "step": "s1",
            "preserve": ["header-order", "casing"]
        }]);
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// A settle-time counter that says nothing, or contradicts itself, would pass
/// every run — `CountBound::is_satisfiable` existed but nothing called it.
#[test]
fn a_background_counter_must_state_a_bound_that_can_fail() {
    let policy = |count: Value| {
        move |d: &mut Value| {
            d["actors"][1]["background"] = json!([
                { "match": { "method": "OPTIONS" }, "respond": { "status": 200 }, "count": count }
            ]);
        }
    };
    assert_fires("background/count-empty", policy(json!({})));
    assert_fires("background/count-unsatisfiable", policy(json!({ "exactly": 1, "at_least": 1 })));
    assert_fires("background/count-unsatisfiable", policy(json!({ "at_least": 3, "at_most": 1 })));
    for good in
        [json!({ "at_least": 1 }), json!({ "exactly": 0 }), json!({ "at_least": 1, "at_most": 3 })]
    {
        let report = broken(policy(good.clone()));
        assert!(!report.has_errors(), "{good}:\n{}", report.render());
    }
    // Omitting `count` answers the traffic and asserts nothing about it.
    let report = broken(|d| {
        d["actors"][1]["background"] =
            json!([{ "match": { "method": "OPTIONS" }, "respond": { "status": 200 } }]);
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// A later assertion cites a branch by name, so two branches cannot share one.
#[test]
fn two_branches_of_one_alt_cannot_share_a_name() {
    assert_fires(
        "alt/duplicate-branch-name",
        with_alt(json!([
            { "name": "same", "steps": [branch_step("s3", 200)] },
            { "name": "same", "steps": [branch_step("s4", 487)] }
        ])),
    );
}

/// §6.3: an auto EXPECT records, an auto SEND is checked by nothing at all —
/// the spec once said "an auto step always carries check: record", which would
/// have had an implementer writing documents lint refuses.
#[test]
fn an_automatic_send_carries_no_check() {
    assert_fires("check/on-send", |d| {
        flow(d).push(json!({
            "id": "s3", "leg": "A", "op": "send", "auto": true, "check": "record",
            "msg": { "method": "ACK", "cseq": 1 },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
    });
    let report = broken(|d| {
        rejected(d);
        flow(d).push(json!({
            "id": "s4", "leg": "A", "op": "send", "auto": true,
            "msg": { "method": "ACK", "cseq": 1 },
            "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
        }));
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// §4.1: a `cause` cites the attempt's own dialog-creating final or an actual
/// closer. The leg here answered 200 and was released by the platform's BYE
/// after the FAR END refused a re-negotiation; citing that in-dialog 488 reads
/// as "this callee answered 488", which is not what happened.
#[test]
fn a_cause_may_not_cite_an_in_dialog_final() {
    let released = |d: &mut Value| {
        d["calls"][0]["attempts"][0]["cause"] = json!("external:488");
        answered(d);
        flow(d).push(json!({
            "id": "s5", "leg": "B", "op": "expect", "check": "assert", "in_dialog": true,
            "msg": { "status": 488, "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
        }));
    };
    assert_fires("cause/in-dialog-final", released);

    // The same 488 answering the attempt's OWN dialog-creating INVITE is the
    // cause it reads as, and lints clean.
    let report = broken(|d| {
        d["calls"][0]["attempts"][0]["cause"] = json!("external:488");
        flow(d).push(json!({
            "id": "s3", "leg": "B", "op": "expect", "check": "assert",
            "msg": { "status": 488, "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// §6.1: the marking is TOTAL. A BYE after the dialog-creating 200 that states
/// no `in_dialog` is an error, because a leg marked on some of its post-final
/// messages and not on others says nothing about any of them.
#[test]
fn a_message_after_the_dialog_creating_final_states_the_marker() {
    assert_fires("in-dialog/missing", |d| {
        answered(d);
        flow(d).push(json!({
            "id": "s5", "leg": "B", "op": "expect", "check": "assert",
            "msg": { "method": "BYE" },
            "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
        }));
    });

    let report = broken(|d| {
        answered(d);
        flow(d).push(json!({
            "id": "s5", "leg": "B", "op": "expect", "check": "assert", "in_dialog": true,
            "msg": { "method": "BYE" },
            "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
        }));
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// The ACK to the dialog-creating 2xx is IN-DIALOG: the dialog exists the moment
/// that final's To-tag arrives (RFC 3261 §13.2.2.4), and §17.1.1.3's
/// transaction-owned ACK is the one to a non-2xx final.
#[test]
fn the_ack_to_the_dialog_creating_final_is_marked() {
    assert_fires("in-dialog/missing", |d| {
        answered(d);
        flow(d)[3]["in_dialog"] = json!(false);
    });
}

/// The other half: a message from before the dialog existed may not claim it.
/// The base document never answers, so nothing on it is in-dialog.
#[test]
fn a_message_before_the_dialog_may_not_claim_it() {
    assert_fires("in-dialog/outside-dialog", |d| {
        flow(d)[1]["in_dialog"] = json!(true);
    });
    assert_fires("in-dialog/outside-dialog", |d| {
        answered(d);
        flow(d)[2]["in_dialog"] = json!(true);
    });
}

/// A CANCEL is scoped to the INVITE transaction it cancels (RFC 3261 §9.1) and
/// is never sent within a dialog (§12.2), so a 200 to one that crossed the 200
/// to the INVITE stays unmarked however late it lands.
#[test]
fn a_cancel_that_crossed_the_answer_is_not_in_dialog() {
    let crossing = |marked: bool| {
        move |d: &mut Value| {
            answered(d);
            flow(d).push(json!({
                "id": "s5", "leg": "B", "op": "send",
                "in_dialog": marked,
                "msg": { "status": 200, "reason": "OK", "cseq-method": "CANCEL" },
                "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
            }));
        }
    };
    assert_fires("in-dialog/outside-dialog", crossing(true));
    let report = broken(crossing(false));
    assert!(!report.has_errors(), "{}", report.render());
}

/// An `alt` branch reads only the dialog-creating finals its OWN run reaches:
/// the 487 branch of a cancel race never saw the answered branch's 200, so
/// nothing in it is in-dialog.
#[test]
fn an_alt_branch_reads_only_the_dialogs_its_own_run_opened() {
    let race = |marked: bool| {
        move |d: &mut Value| {
            flow(d).push(json!({
                "id": "a1", "op": "alt", "branches": [
                    { "name": "answered", "steps": [
                        { "id": "s3", "leg": "B", "op": "expect", "check": "assert",
                          "msg": { "status": 200, "cseq-method": "INVITE" },
                          "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false } },
                        { "id": "s4", "leg": "B", "op": "send", "auto": true, "in_dialog": true,
                          "confirms_dialog": true,
                          "msg": { "method": "ACK", "cseq": 1 },
                          "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false } }
                    ] },
                    { "name": "cancelled", "steps": [
                        { "id": "s5", "leg": "B", "op": "expect", "check": "assert",
                          "msg": { "status": 487, "cseq-method": "INVITE" },
                          "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false } },
                        { "id": "s6", "leg": "B", "op": "send", "auto": true, "in_dialog": marked,
                          "msg": { "method": "ACK", "cseq": 1 },
                          "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false } }
                    ] }
                ]
            }));
        }
    };
    assert_fires("in-dialog/outside-dialog", race(true));
    let report = broken(race(false));
    assert!(!report.has_errors(), "{}", report.render());
}

// --- the confirming ACK -------------------------------------------------

/// §6.1: the ACK answering a dialog-creating final SAYS that it does. Dropping
/// the marker leaves the document unable to tell that ACK from a re-INVITE's,
/// which is the whole reason the marker exists.
#[test]
fn the_ack_answering_the_dialog_creating_final_states_confirms_dialog() {
    assert_fires("in-dialog/confirm-missing", |d| {
        answered(d);
        flow(d)[3]["confirms_dialog"] = json!(false);
    });

    let report = broken(answered);
    assert!(!report.has_errors(), "{}", report.render());
}

/// An ACK to a re-INVITE's 2xx is an ordinary in-dialog ACK: it renegotiates a
/// dialog that is already up, and confirms nothing.
#[test]
fn a_re_invite_ack_does_not_confirm_the_dialog() {
    assert_fires("in-dialog/confirm-not-the-answer", |d| {
        reinvited(d);
        flow(d)[3]["confirms_dialog"] = json!(false);
        flow(d)[7]["confirms_dialog"] = json!(true);
    });

    let report = broken(reinvited);
    assert!(!report.has_errors(), "{}", report.render());
}

/// One dialog is confirmed once. Marking the re-INVITE's ACK BESIDE the real
/// one is the same defect read from the other end.
#[test]
fn one_dialog_carries_one_confirming_ack() {
    assert_fires("in-dialog/confirm-duplicate", |d| {
        reinvited(d);
        flow(d)[7]["confirms_dialog"] = json!(true);
    });
}

/// The marker is an ACK's, and an ACK's to a 2xx: a response cannot carry it,
/// and neither can the transaction-owned ACK to a non-2xx final (RFC 3261
/// §17.1.1.3), which runs on a leg that may hold no dialog at all.
#[test]
fn only_an_ack_inside_a_confirmed_dialog_may_confirm_it() {
    assert_fires("in-dialog/confirm-not-ack", |d| {
        answered(d);
        flow(d)[2]["confirms_dialog"] = json!(true);
    });
    assert_fires("in-dialog/confirm-outside-dialog", |d| {
        flow(d).push(json!({
            "id": "s3", "leg": "B", "op": "send",
            "msg": { "status": 486, "reason": "Busy Here", "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
        flow(d).push(json!({
            "id": "s4", "leg": "B", "op": "expect", "check": "record", "auto": true,
            "confirms_dialog": true,
            "msg": { "method": "ACK", "cseq": 1 },
            "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
        }));
    });
}

/// A re-INVITE sent over the un-ACKed dialog-creating 2xx is answered 491
/// (RFC 3261 §14.1) and its ACK is that transaction's own (§17.1.1.3): the ACK
/// confirming the dialog is the one answering the 2xx (§13.2.2.4), which here
/// is the SECOND ACK the leg carries after it. Pinned from both sides of the
/// leg: the actor that took the INVITE and sent the finals (`B`), and the one
/// that sent the INVITE and took them (`A`).
#[test]
fn the_ack_to_the_2xx_confirms_the_dialog_not_a_491_rounds_ack_before_it() {
    let over_unacked = |leg: &'static str, on_491_ack: bool, on_2xx_ack: bool| {
        // `A` sends the INVITEs and ACKs and takes the finals; `B` the reverse.
        let (req, resp) = if leg == "A" { ("send", "expect") } else { ("expect", "send") };
        let check = |op: &str| if op == "expect" { json!("assert") } else { Value::Null };
        let record = |op: &str| if op == "expect" { json!("record") } else { Value::Null };
        move |d: &mut Value| {
            for step in [
                json!({
                    "id": "s3", "leg": leg, "op": resp, "check": check(resp),
                    "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s4", "leg": leg, "op": req, "check": check(req), "in_dialog": true,
                    "msg": { "method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s5", "leg": leg, "op": resp, "check": check(resp), "in_dialog": true,
                    "msg": { "status": 491, "reason": "Request Pending", "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s6", "leg": leg, "op": req, "check": record(req), "auto": true,
                    "in_dialog": true, "confirms_dialog": on_491_ack,
                    "msg": { "method": "ACK", "cseq": 2 },
                    "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s7", "leg": leg, "op": req, "check": record(req), "auto": true,
                    "in_dialog": true, "confirms_dialog": on_2xx_ack,
                    "msg": { "method": "ACK", "cseq": 1 },
                    "delay": { "ms": 0, "from": "step:s6", "compressible": true, "timer_linked": false }
                }),
            ] {
                let mut step = step;
                if step["check"].is_null() {
                    step.as_object_mut().expect("a step").remove("check");
                }
                flow(d).push(step);
            }
        }
    };
    for leg in ["A", "B"] {
        let report = broken(over_unacked(leg, false, true));
        assert!(!report.has_errors(), "leg {leg}:\n{}", report.render());
        assert_fires("in-dialog/confirm-not-the-answer", over_unacked(leg, true, false));
        assert_fires("in-dialog/confirm-missing", over_unacked(leg, true, false));
        assert_fires("in-dialog/confirm-missing", over_unacked(leg, false, false));
    }
}

/// The pairing is by position on the leg, with no CSeq to read: a 2xx
/// re-emitted AFTER a later INVITE of its direction was answered lands on that
/// INVITE, so an authored document states the repeat as the folded step's
/// `retransmits` (§6.9) — as the cut does — and not as a step of its own.
#[test]
fn a_re_emitted_2xx_is_folded_onto_the_step_it_repeats_not_stated_after_a_newer_invite() {
    let round_491 = |d: &mut Value| {
        for step in [
            json!({
                "id": "s3", "leg": "B", "op": "send",
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s4", "leg": "B", "op": "expect", "check": "assert", "in_dialog": true,
                "msg": { "method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s5", "leg": "B", "op": "send", "in_dialog": true,
                "msg": { "status": 491, "reason": "Request Pending", "cseq-method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s6", "leg": "B", "op": "expect", "check": "record", "auto": true, "in_dialog": true,
                "msg": { "method": "ACK", "cseq": 2 },
                "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false }
            }),
        ] {
            flow(d).push(step);
        }
    };
    let confirming = |d: &mut Value, from: &str| {
        flow(d).push(json!({
            "id": "s8", "leg": "B", "op": "expect", "check": "record", "auto": true,
            "in_dialog": true, "confirms_dialog": true,
            "msg": { "method": "ACK", "cseq": 1 },
            "delay": { "ms": 0, "from": from, "compressible": true, "timer_linked": false }
        }));
    };
    let report = broken(|d| {
        round_491(d);
        flow(d)[2]["retransmits"] = json!(1);
        flow(d)[2]["retransmit_intervals_ms"] = json!([500]);
        confirming(d, "step:s6");
    });
    assert!(!report.has_errors(), "{}", report.render());
    assert_fires("in-dialog/confirm-not-the-answer", |d| {
        round_491(d);
        flow(d).push(json!({
            "id": "s7", "leg": "B", "op": "send", "in_dialog": true,
            "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s6", "compressible": true, "timer_linked": false }
        }));
        confirming(d, "step:s7");
    });
}

/// Under forking each answered fork mints its own dialog on the one leg, and
/// each is confirmed by the ACK that names it. The fork tag is what pairs the
/// two: `early` and `confirms_dialog` ride the same ACK, saying different
/// things.
#[test]
fn each_answered_fork_carries_its_own_confirming_ack() {
    let forked = |second_marked: bool| {
        move |d: &mut Value| {
            for step in [
                json!({
                    "id": "s3", "leg": "B", "op": "send", "early": "f1",
                    "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s4", "leg": "B", "op": "expect", "check": "record", "auto": true,
                    "in_dialog": true, "confirms_dialog": true, "early": "f1",
                    "msg": { "method": "ACK", "cseq": 1 },
                    "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s5", "leg": "B", "op": "send", "early": "f2", "in_dialog": true,
                    "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s6", "leg": "B", "op": "expect", "check": "record", "auto": true,
                    "in_dialog": true, "confirms_dialog": second_marked, "early": "f2",
                    "msg": { "method": "ACK", "cseq": 1 },
                    "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false }
                }),
            ] {
                flow(d).push(step);
            }
        }
    };
    let report = broken(forked(true));
    assert!(!report.has_errors(), "{}", report.render());
    assert_fires("in-dialog/confirm-missing", forked(false));
}

/// Two forks answered before either ACK arrives: the fork tag, not the order
/// of the ACKs, pairs each ACK with the 2xx it answers, in either order.
#[test]
fn two_forks_answered_at_once_are_each_confirmed_by_the_ack_naming_them() {
    let interleaved = |first: &'static str, second: &'static str, second_marked: bool| {
        move |d: &mut Value| {
            for step in [
                json!({
                    "id": "s3", "leg": "B", "op": "send", "early": "f1",
                    "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s4", "leg": "B", "op": "send", "early": "f2", "in_dialog": true,
                    "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                    "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s5", "leg": "B", "op": "expect", "check": "record", "auto": true,
                    "in_dialog": true, "confirms_dialog": true, "early": first,
                    "msg": { "method": "ACK", "cseq": 1 },
                    "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
                }),
                json!({
                    "id": "s6", "leg": "B", "op": "expect", "check": "record", "auto": true,
                    "in_dialog": true, "confirms_dialog": second_marked, "early": second,
                    "msg": { "method": "ACK", "cseq": 1 },
                    "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false }
                }),
            ] {
                flow(d).push(step);
            }
        }
    };
    for (first, second) in [("f1", "f2"), ("f2", "f1")] {
        let report = broken(interleaved(first, second, true));
        assert!(!report.has_errors(), "{first} then {second}:\n{}", report.render());
        assert_fires("in-dialog/confirm-missing", interleaved(first, second, false));
    }
}

/// The closer a `closed:bye` cites is a datagram of the flow, not a reading.
#[test]
fn a_closed_bye_cause_names_a_bye_the_flow_carries() {
    assert_fires("cause/closer-missing", |d| {
        d["calls"][0]["attempts"][0]["cause"] = json!("closed:bye");
    });

    let report = broken(|d| {
        d["calls"][0]["attempts"][0]["cause"] = json!("closed:bye");
        flow(d).push(json!({
            "id": "s3", "leg": "B", "op": "expect", "check": "assert",
            "msg": { "method": "BYE" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// §11.1: an `rfc_violations` entry lands on a step of this flow and names an
/// emitter this document declares.
#[test]
fn an_rfc_violation_anchors_on_a_step_and_names_an_emitter() {
    assert_fires("ref/violation-step-unknown", |d| {
        d["rfc_violations"] =
            json!([{ "rule": "no-200-after-cancel", "step": "s99", "emitter": "uas1" }]);
    });
    assert_fires("ref/violation-emitter-unknown", |d| {
        d["rfc_violations"] =
            json!([{ "rule": "no-200-after-cancel", "step": "s2", "emitter": "uas9" }]);
    });

    // An actor of the document, or the system under test, and both lint clean.
    for emitter in ["uas1", "sut"] {
        let report = broken(|d| {
            d["rfc_violations"] =
                json!([{ "rule": "no-200-after-cancel", "step": "s2", "emitter": emitter }]);
        });
        assert!(!report.has_errors(), "{}", report.render());
    }
}

/// §11.1: the rule vocabulary is closed, so a document cannot name a rule no
/// detector can decide.
#[test]
fn an_rfc_violation_rule_outside_the_vocabulary_is_refused() {
    assert_fires("schema/parse", |d| {
        d["rfc_violations"] =
            json!([{ "rule": "answer-after-cancel", "step": "s2", "emitter": "uas1" }]);
    });
}

/// §9.1: a check class only means something beside an origin lane — without one
/// there is no lane to compare against, and the class gates everywhere.
#[test]
fn a_classified_check_without_an_origin_lane_is_flagged() {
    assert_fires("lane/class-without-origin", |d| {
        d["postconditions"]["cdr"]["checks"] = json!([{ "field": "events", "op": "regex", "value": "InviteReceived",
                     "class": "cdr-vocabulary" }]);
    });

    let report = broken(|d| {
        d["case"]["origin_lane"] = json!("origin-platform");
        d["postconditions"]["cdr"]["checks"] = json!([{ "field": "events", "op": "regex", "value": "InviteReceived",
                     "class": "cdr-vocabulary" }]);
        flow(d)[1]["msg"]["headers"] = json!([
            { "name": "P-Charging-Vector", "value": "icid-value=x",
              "class": "origin-platform-header" }
        ]);
    });
    assert!(report.diagnostics.is_empty(), "{}", report.render());
}

// --- must_fail: the declared failure of a negative case ------------------

/// Leg B's dialog-creating 200 (`s3`), with NO ACK behind it: the shape a
/// capture holds when the source platform relayed an ACK that never came.
fn unacked(document: &mut Value) {
    flow(document).push(json!({
        "id": "s3", "leg": "B", "op": "send",
        "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
        "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
    }));
}

/// One `unexpected-ack` declaration on `step`.
fn declares(step: &str) -> Value {
    json!([{ "failure": "unexpected-ack", "step": step,
             "derived_from": "no-ack-to-dialog-creating-2xx" }])
}

/// §11.2: the declaration is a coordinate into this flow. One that lands
/// nowhere makes the case unpassable — no run can produce a failure at a step
/// the document does not have.
#[test]
fn a_declared_failure_anchors_on_a_step_of_this_flow() {
    assert_fires("ref/must-fail-step-unknown", |d| {
        unacked(d);
        d["must_fail"] = declares("s99");
    });

    let report = broken(|d| {
        unacked(d);
        d["must_fail"] = declares("s3");
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// `unexpected-ack` is about the PLATFORM's ACK, so it belongs on the 2xx the
/// scripted peer SENDS. On a 2xx the peer merely receives, the withheld ACK is
/// the peer's own and nothing unexpected ever arrives.
#[test]
fn unexpected_ack_is_declared_on_the_2xx_the_peer_sends() {
    assert_fires("must-fail/anchor-not-a-2xx-send", |d| {
        unacked(d);
        flow(d)[2]["op"] = json!("expect");
        flow(d)[2]["check"] = json!("record");
        d["must_fail"] = declares("s3");
    });
    assert_fires("must-fail/anchor-not-a-2xx-send", |d| {
        unacked(d);
        d["must_fail"] = declares("s1");
    });
    assert_fires("must-fail/anchor-not-a-2xx-send", |d| {
        unacked(d);
        flow(d)[2]["msg"] =
            json!({ "status": 486, "reason": "Busy Here", "cseq-method": "INVITE" });
        d["must_fail"] = declares("s3");
    });
}

/// A document cannot both expect an ACK and declare it unexpected: the ACK step
/// is the run being satisfied. This is the guard that keeps a negative case
/// from going green because the failure it declares can no longer happen.
#[test]
fn a_2xx_the_flow_already_acks_declares_no_unexpected_ack() {
    assert_fires("must-fail/anchor-already-acked", |d| {
        answered(d);
        d["must_fail"] = declares("s3");
    });
}

/// The ACK that answers the declaration is the one DISCHARGING the anchor's
/// transaction (RFC 3261 §13.2.2.4): an ACK the leg expects for a LATER INVITE
/// transaction it takes — a re-INVITE's — leaves the anchor's 2xx exactly as
/// un-ACKed as the capture had it, so the declaration stands beside it.
#[test]
fn an_ack_expected_for_a_later_re_invite_leaves_the_anchor_unacked() {
    assert_clean("must-fail/anchor-already-acked", |d| {
        unacked(d);
        for step in [
            json!({
                "id": "s4", "leg": "B", "op": "expect", "check": "record", "in_dialog": true,
                "msg": { "method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s5", "leg": "B", "op": "send", "in_dialog": true,
                "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
                "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
            }),
            json!({
                "id": "s6", "leg": "B", "op": "expect", "check": "record", "auto": true,
                "in_dialog": true,
                "msg": { "method": "ACK", "cseq": 2 },
                "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false }
            }),
        ] {
            flow(d).push(step);
        }
        d["must_fail"] = declares("s3");
    });
}

/// A run produces the failure once, and the verdict compares declared against
/// observed: a second copy would demand a second occurrence nobody predicted.
#[test]
fn one_failure_is_declared_once_per_anchor() {
    assert_fires("must-fail/duplicate", |d| {
        unacked(d);
        let mut twice = declares("s3");
        let entry = twice[0].clone();
        twice.as_array_mut().expect("declarations").push(entry);
        d["must_fail"] = twice;
    });
}

/// A failure token outside the closed vocabulary never reaches lint: the
/// document does not parse, which is what "closed" is for.
#[test]
fn a_declared_failure_outside_the_vocabulary_is_refused() {
    assert_fires("schema/parse", |d| {
        unacked(d);
        d["must_fail"] = json!([{ "failure": "unexpected-bye", "step": "s3",
                                  "derived_from": "no-ack-to-dialog-creating-2xx" }]);
    });
    assert_fires("schema/parse", |d| {
        unacked(d);
        d["must_fail"] = json!([{ "failure": "unexpected-ack", "step": "s3",
                                  "derived_from": "peer-was-rude" }]);
    });
}

/// Leg B's 480 (`s3`) with the source's LATE CANCEL behind it (`s4`) — the
/// shape a capture holds when the platform cancelled a transaction it had
/// already taken a final on.
fn late_cancelled(document: &mut Value) {
    flow(document).push(json!({
        "id": "s3", "leg": "B", "op": "send",
        "msg": { "status": 480, "reason": "Temporarily Unavailable", "cseq-method": "INVITE" },
        "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
    }));
    flow(document).push(json!({
        "id": "s4", "leg": "B", "op": "expect", "check": "record",
        "msg": { "method": "CANCEL" },
        "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
    }));
}

/// One `unexpected-cancel` declaration on `step`.
fn declares_cancel(step: &str) -> Value {
    json!([{ "failure": "unexpected-cancel", "step": step,
             "derived_from": "no-cancel-after-final" }])
}

/// `unexpected-cancel` is about the PLATFORM's prompt CANCEL, so it belongs on
/// the FINAL the source's own CANCEL arrived behind — a send, and the emission
/// that supplies the dialog. A provisional ends no transaction, and an expect
/// is a final this platform sent rather than took.
#[test]
fn unexpected_cancel_is_declared_on_the_final_the_peer_sends() {
    let report = broken(|d| {
        late_cancelled(d);
        d["must_fail"] = declares_cancel("s3");
    });
    assert!(!report.has_errors(), "{}", report.render());

    assert_fires("must-fail/anchor-not-a-final-send", |d| {
        late_cancelled(d);
        flow(d)[2]["op"] = json!("expect");
        flow(d)[2]["check"] = json!("record");
        d["must_fail"] = declares_cancel("s3");
    });
    assert_fires("must-fail/anchor-not-a-final-send", |d| {
        late_cancelled(d);
        flow(d)[2]["msg"] = json!({ "status": 180, "reason": "Ringing",
                                    "cseq-method": "INVITE" });
        d["must_fail"] = declares_cancel("s3");
    });
}

/// A document that states no CANCEL behind the final states no lateness, so
/// there is nothing this platform's own CANCEL could arrive ahead of.
#[test]
fn a_final_with_no_cancel_behind_it_declares_no_unexpected_cancel() {
    assert_fires("must-fail/anchor-has-no-late-cancel", |d| {
        unacked(d);
        d["must_fail"] = declares_cancel("s3");
    });
}

/// A CANCEL the flow already expects AHEAD of the final is the run being
/// satisfied: the platform emitting one there matches a step, and a declaration
/// nothing can produce makes the case unpassable.
#[test]
fn a_cancel_the_flow_already_expects_in_time_declares_no_unexpected_cancel() {
    assert_fires("must-fail/anchor-already-cancelled", |d| {
        flow(d).push(json!({
            "id": "s3", "leg": "B", "op": "expect", "check": "record",
            "msg": { "method": "CANCEL" },
            "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
        }));
        flow(d).push(json!({
            "id": "s4", "leg": "B", "op": "send",
            "msg": { "status": 487, "reason": "Request Terminated", "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
        }));
        flow(d).push(json!({
            "id": "s5", "leg": "B", "op": "expect", "check": "record",
            "msg": { "method": "CANCEL" },
            "delay": { "ms": 0, "from": "step:s4", "compressible": true, "timer_linked": false }
        }));
        d["must_fail"] = declares_cancel("s4");
    });
}

/// Leg B's reliable 183 (`s3`), with NO PRACK behind it: the shape a capture
/// holds when the source platform never PRACKed the provisional it took.
fn unpracked(document: &mut Value) {
    flow(document).push(json!({
        "id": "s3", "leg": "B", "op": "send",
        "msg": { "status": 183, "reason": "Session Progress", "cseq-method": "INVITE",
                 "headers": [ { "name": "Require", "value": "100rel" },
                              { "name": "RSeq", "value": "1" } ] },
        "delay": { "ms": 0, "from": "step:s2", "compressible": true, "timer_linked": false }
    }));
}

/// One `unexpected-prack` declaration on `step`.
fn declares_prack(step: &str) -> Value {
    json!([{ "failure": "unexpected-prack", "step": step,
             "derived_from": "unacked-reliable-provisional" }])
}

/// `unexpected-prack` is about the PLATFORM's PRACK, so it belongs on the
/// reliable provisional the scripted peer SENDS. A plain provisional draws no
/// PRACK, and on one the peer merely receives the withheld PRACK is its own.
#[test]
fn unexpected_prack_is_declared_on_the_reliable_provisional_the_peer_sends() {
    let report = broken(|d| {
        unpracked(d);
        d["must_fail"] = declares_prack("s3");
    });
    assert!(!report.has_errors(), "{}", report.render());

    assert_fires("must-fail/anchor-not-a-reliable-provisional-send", |d| {
        unpracked(d);
        flow(d)[2]["op"] = json!("expect");
        flow(d)[2]["check"] = json!("record");
        d["must_fail"] = declares_prack("s3");
    });
    assert_fires("must-fail/anchor-not-a-reliable-provisional-send", |d| {
        unacked(d);
        d["must_fail"] = declares_prack("s3");
    });
    assert_fires("must-fail/anchor-not-a-reliable-provisional-send", |d| {
        unpracked(d);
        flow(d)[2]["msg"] = json!({ "status": 183, "reason": "Session Progress",
                                    "cseq-method": "INVITE" });
        d["must_fail"] = declares_prack("s3");
    });
}

/// A document cannot both expect a PRACK and declare it unexpected: the PRACK
/// step is the run being satisfied.
#[test]
fn a_provisional_the_flow_already_pracks_declares_no_unexpected_prack() {
    assert_fires("must-fail/anchor-already-pracked", |d| {
        unpracked(d);
        flow(d).push(json!({
            "id": "s4", "leg": "B", "op": "expect", "check": "record",
            "msg": { "method": "PRACK" },
            "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
        }));
        d["must_fail"] = declares_prack("s3");
    });
}

/// The ACK that suppresses a declaration is one the anchor's OWN leg states:
/// an ACK on another leg confirms another dialog, and the platform's ACK to
/// this 2xx is still the failure the run owes.
#[test]
fn an_ack_on_another_leg_suppresses_nothing() {
    let report = broken(|d| {
        unacked(d);
        flow(d).push(json!({
            "id": "s5", "leg": "A", "op": "expect", "check": "record",
            "msg": { "status": 200, "reason": "OK", "cseq-method": "INVITE" },
            "delay": { "ms": 0, "from": "step:s3", "compressible": true, "timer_linked": false }
        }));
        flow(d).push(json!({
            "id": "s6", "leg": "A", "op": "send", "in_dialog": true, "confirms_dialog": true,
            "msg": { "method": "ACK" },
            "delay": { "ms": 0, "from": "step:s5", "compressible": true, "timer_linked": false }
        }));
        d["must_fail"] = declares("s3");
    });
    assert!(!report.has_errors(), "{}", report.render());
}

/// §13.1: a declaration is a reading OF the capture plus this lane's own
/// behaviour, exactly as §13.2's adaptation is, so the subset gate lets it
/// through on a captured document.
#[test]
fn a_captured_document_may_declare_the_failure_its_replay_will_produce() {
    let report = broken(|d| {
        unacked(d);
        d["case"]["origin"] = json!("capture");
        d["case"]["source"] =
            json!({ "capture": "c.pcap", "call_groups": [0], "anonymized": true });
        d["timing"]["capture_span_ms"] = json!(120);
        for (i, step) in flow(d).iter_mut().enumerate() {
            step["observed"] = json!({ "leg": 0, "msg": i, "at_us": i * 10 });
        }
        d["must_fail"] = declares("s3");
    });
    assert!(!report.has_errors(), "{}", report.render());
}
