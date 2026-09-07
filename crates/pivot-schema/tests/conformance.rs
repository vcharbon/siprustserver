//! Fixture conformance: every shipped pivot parses through the typed model,
//! re-serializes BYTE-IDENTICALLY under the canonical formatter, and lints
//! clean.
//!
//! The eleven `capture` fixtures are real generated documents, emitted by the
//! TypeScript generator and then scrubbed of the third-party operator, vendor
//! and node identifiers a number-anonymizer does not touch. Substitution is
//! textual, so each file still carries the emitter's own byte layout:
//! byte-equality here is the two implementations agreeing on §2.1, not this
//! crate agreeing with itself.
//!
//! The `authored` fixtures are hand-written, and they are here for the half of
//! the format a generator may never emit: `alt`, `unordered`, `optional`,
//! `inject`, `after`, background policies, accessors, joined legs, several
//! calls at once and postcondition checks. Between them the set covers every
//! construct v3 has.
//!
//! `schedules/table.json` is the third kind of fixture: the retransmission
//! schedule table `pivot-schema schedules` prints, byte for byte. It sits in a
//! directory of its own because every flat `.json` here is read as a pivot
//! document (`pivot-interpreter`'s corpus walk). Its mirror in
//! `ts/contracts/src/schedules.ts` is checked against the binary, so a change
//! to `sip-retransmit`'s classes lands here first and in the mirror second.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pivot_schema::body::Body;
use pivot_schema::case::{LaneVerdict, Origin};
use pivot_schema::flow::{CheckMode, FlowNode, Op};
use pivot_schema::{PIVOT_VERSION, PivotV3};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixtures() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(fixture_dir())
        .expect("the fixture directory is readable")
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.to_string_lossy().ends_with(".v3.json"))
        .map(|path| {
            let name = path.file_name().expect("a named file").to_string_lossy().into_owned();
            (name, std::fs::read_to_string(&path).expect("a readable fixture"))
        })
        .collect();
    out.sort();
    assert_eq!(out.len(), 14, "the fixture set changed size");
    out
}

fn parsed() -> Vec<(String, PivotV3)> {
    fixtures()
        .into_iter()
        .map(|(name, text)| {
            let pivot = PivotV3::from_json(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, pivot)
        })
        .collect()
}

#[test]
fn every_fixture_round_trips_byte_identically() {
    for (name, text) in fixtures() {
        let pivot = PivotV3::from_json(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(pivot.pivot_version, PIVOT_VERSION, "{name}");
        assert_eq!(pivot.to_canonical_json(), text, "{name}: re-serialized bytes differ");
    }
}

#[test]
fn re_parsing_a_re_serialized_fixture_yields_the_same_document() {
    for (name, pivot) in parsed() {
        let twice = PivotV3::from_json(&pivot.to_canonical_json()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(pivot, twice, "{name}");
    }
}

/// Lint is part of the contract, not a separate opinion: a shipped fixture that
/// lint refuses would mean the two disagree about what a valid document is.
#[test]
fn every_fixture_lints_clean() {
    for (name, pivot) in parsed() {
        let report = pivot_schema::lint(&pivot);
        assert!(!report.has_errors(), "{name}:\n{}", report.render());
    }
}

/// §2.2: no field's emptiness carries meaning, so an empty collection is never
/// written. A regression here would be silent — the document still parses.
#[test]
fn no_fixture_writes_an_empty_collection() {
    for (name, text) in fixtures() {
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(empty_paths(&value, "").is_empty(), "{name}: {:?}", empty_paths(&value, ""));
    }
}

fn empty_paths(value: &serde_json::Value, at: &str) -> Vec<String> {
    match value {
        serde_json::Value::Object(map) if map.is_empty() => vec![at.to_string()],
        serde_json::Value::Array(items) if items.is_empty() => vec![at.to_string()],
        serde_json::Value::Object(map) => {
            map.iter().flat_map(|(k, v)| empty_paths(v, &format!("{at}.{k}"))).collect()
        }
        serde_json::Value::Array(items) => {
            items.iter().flat_map(|v| empty_paths(v, &format!("{at}[]"))).collect()
        }
        _ => Vec::new(),
    }
}

/// §6.1 and §6.2: an auto step states its captured `cseq` and always records.
/// `cseq` is scoped to auto steps alone — it is a pairing token, and copying it
/// onto a scripted step would invite a lane to replay it. Its CONTENT is the
/// three-tier build every step gets (§6.3), so nothing here asks it to be bare.
#[test]
fn auto_steps_carry_a_cseq_and_record_while_scripted_steps_carry_neither() {
    let mut seen_auto = 0;
    for (name, pivot) in parsed() {
        for step in pivot.steps() {
            let at = format!("{name} step {}", step.id);
            if step.auto {
                seen_auto += 1;
                assert!(step.msg.cseq.is_some(), "{at}: an auto step states its captured CSeq");
                if step.op == Op::Expect {
                    assert_eq!(step.check, Some(CheckMode::Record), "{at}");
                }
            } else {
                assert!(step.msg.cseq.is_none(), "{at}: cseq is an auto-step field");
            }
            // `check` exists on expects: a send is emitted, not checked.
            assert_eq!(step.check.is_some(), step.op == Op::Expect, "{at}");
        }
    }
    assert!(seen_auto > 50, "the fixtures exercise auto steps");
}

/// §6: every step carries a unique id, and every dwell anchors on `trigger` or
/// on a step that has already run — an anchor pointing forward would make the
/// dwell underivable.
#[test]
fn step_ids_are_unique_and_every_delay_anchors_backwards() {
    for (name, pivot) in parsed() {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for step in pivot.steps() {
            assert!(seen.insert(&step.id), "{name}: step id {:?} is claimed twice", step.id);
        }
        for (position, node) in pivot.flow.iter().enumerate() {
            for (inner, step) in node.steps().into_iter().enumerate() {
                let Some(anchor) = step.delay.from.step() else { continue };
                let anchored_at = pivot
                    .flow
                    .iter()
                    .enumerate()
                    .find_map(|(p, n)| {
                        n.steps().iter().position(|s| s.id == anchor).map(|i| (p, i))
                    })
                    .unwrap_or_else(|| panic!("{name}: anchor {anchor} resolves"));
                assert!(anchored_at < (position, inner), "{name} step {}: anchor is not earlier", step.id);
            }
        }
    }
}

/// Every id a document references resolves inside the same document. Lint owns
/// the rule; this pins that the SHIPPED set exercises it rather than trivially
/// satisfying it.
#[test]
fn every_reference_resolves_within_the_document() {
    let mut cross_references = 0;
    for (name, pivot) in parsed() {
        let legs: BTreeSet<&str> = pivot.legs.iter().map(|l| l.id.as_str()).collect();
        let steps: BTreeSet<&str> = pivot.steps().iter().map(|s| s.id.as_str()).collect();
        for call in &pivot.calls {
            assert!(legs.contains(call.caller_leg.as_str()), "{name}: call {}", call.id);
            for attempt in &call.attempts {
                assert!(legs.contains(attempt.leg.as_str()), "{name}: attempt leg {}", attempt.leg);
                assert!(attempt.no_answer_ms_is_declarable(), "{name}: attempt on leg {}", attempt.leg);
            }
        }
        for node in &pivot.flow {
            for target in node.after() {
                assert!(steps.contains(target.as_str()) || pivot.flow.iter().any(|n| n.id() == target), "{name}: after {target}");
                cross_references += 1;
            }
        }
        for step in pivot.steps() {
            assert!(legs.contains(step.leg.as_str()), "{name}: step {} leg", step.id);
            cross_references += usize::from(!step.after.is_empty());
        }
    }
    assert!(cross_references > 0, "the fixture set no longer exercises `after`");
}

/// §4: `(branch, position)` is the chain's stable key, so no two attempts of one
/// call may share one.
#[test]
fn attempt_positions_are_unique_within_their_branch() {
    for (name, pivot) in parsed() {
        for call in &pivot.calls {
            let keys: BTreeSet<(u32, u32)> =
                call.attempts.iter().map(|a| (a.branch, a.position)).collect();
            assert_eq!(keys.len(), call.attempts.len(), "{name}: call {}", call.id);
        }
    }
}

/// The feature inventory the set is chosen to hold. A fixture swap that loses
/// one of these silently narrows what the round-trip proves.
#[test]
fn the_fixture_set_covers_every_feature_the_format_has() {
    let (mut multipart, mut frozen_body, mut retransmits, mut held_auto) = (0, 0, 0, 0);
    let (mut no_answer, mut chains, mut forks, mut blocked) = (0, 0, 0, 0);
    let (mut relay18x, mut prack_mode, mut deviations, mut defects) = (0, 0, 0, 0);
    let (mut loopback, mut dedicated, mut frozen_ref, mut catalog) = (0, 0, 0, 0);
    let (mut alts, mut unordered, mut injects, mut optionals) = (0, 0, 0, 0);
    let (mut background, mut counters, mut accessors, mut checks) = (0, 0, 0, 0);
    let (mut parallel_calls, mut cdr_counts, mut cdr_absent, mut requires) = (0, 0, 0, 0);
    let (mut media_block, mut relative_cseq, mut authored, mut captured) = (0, 0, 0, 0);
    let (mut joins, mut registered, mut numbered, mut targeted) = (0, 0, 0, 0);
    let (mut part_ids, mut part_headers, mut in_dialog) = (0, 0, 0);
    let (mut part_type_params, mut confirming_acks) = (0, 0);
    let mut families = BTreeSet::new();

    for (_, pivot) in parsed() {
        families.insert(pivot.case.family.clone());
        deviations += pivot.deviations.len();
        defects += usize::from(pivot.case.defect.is_some());
        blocked += pivot.case.lanes.values().filter(|v| !v.is_ok()).count();
        requires += usize::from(!pivot.case.requires.is_empty());
        media_block += usize::from(pivot.media.is_some());
        parallel_calls += usize::from(pivot.calls.len() > 1);
        match pivot.case.origin {
            Origin::Capture => captured += 1,
            Origin::Authored => authored += 1,
        }
        loopback += pivot
            .endpoints
            .iter()
            .filter(|e| e.binding == pivot_schema::placement::Binding::Loopback)
            .count();
        dedicated += pivot.endpoints.len() - loopback.min(pivot.endpoints.len());
        for actor in &pivot.actors {
            background += actor.background.len();
            counters += actor.background.iter().filter(|p| p.count.is_some()).count();
        }
        for call in &pivot.calls {
            relay18x += usize::from(call.relay18x.is_some());
            prack_mode += call.relay18x.iter().filter(|r| r.prack.is_some()).count();
            chains += usize::from(call.attempts.len() > 1);
            forks += usize::from(
                call.attempts.iter().map(|a| a.branch).collect::<BTreeSet<_>>().len() > 1,
            );
            no_answer += call.attempts.iter().filter(|a| a.no_answer_ms.is_some()).count();
            joins += call.attempts.iter().filter(|a| a.joined_by.is_some()).count();
        }
        catalog += pivot.identities.iter().filter(|i| i.catalog.is_some()).count();
        registered += pivot.identities.len();
        for deviation in &pivot.deviations {
            targeted += usize::from(deviation.header.is_some());
            relative_cseq += usize::from(matches!(
                deviation.value,
                Some(pivot_schema::deviation::CseqValue::Relative(_))
            ));
        }
        for node in &pivot.flow {
            match node {
                FlowNode::Alt(alt) => alts += alt.branches.len(),
                FlowNode::Unordered(group) => unordered += group.steps.len(),
                FlowNode::Inject(_) => injects += 1,
                FlowNode::Message(_) => {}
            }
        }
        if let Some(postconditions) = &pivot.postconditions {
            match &postconditions.cdr {
                Some(pivot_schema::postcondition::CdrExpectation::Expected(_)) => cdr_counts += 1,
                Some(pivot_schema::postcondition::CdrExpectation::Absent(_)) => cdr_absent += 1,
                None => {}
            }
            checks += postconditions.checks.len();
        }
        for step in pivot.steps() {
            retransmits += usize::from(step.retransmits.is_some());
            in_dialog += usize::from(step.in_dialog);
            confirming_acks += usize::from(step.confirms_dialog);
            held_auto += usize::from(step.auto && step.delay.ms > 1_000);
            optionals += usize::from(step.optional);
            checks += step.checks.len();
            accessors += step
                .msg
                .headers
                .iter()
                .filter(|h| pivot_schema::accessor::Accessor::present_in(&h.value))
                .count();
            accessors += step
                .checks
                .iter()
                .filter(|c| c.value.as_deref().is_some_and(pivot_schema::accessor::Accessor::present_in))
                .count();
            numbered += step
                .msg
                .headers
                .iter()
                .flat_map(|h| pivot_schema::accessor::Accessor::scan(&h.value))
                .filter(|found| {
                    matches!(found, Ok(pivot_schema::accessor::Accessor::Number { .. }))
                })
                .count();
            match &step.msg.body {
                Some(Body::Multipart(m)) => {
                    multipart += 1;
                    for part in &m.multipart.parts {
                        part_ids += usize::from(part.content_id.is_some());
                        part_headers += part.headers.len();
                        part_type_params += usize::from(part.content_type.contains(';'));
                    }
                }
                Some(Body::Resource(r)) if r.mode.is_some() => frozen_body += 1,
                _ => {}
            }
            for reference in [&step.msg.ruri, &step.msg.from, &step.msg.to].into_iter().flatten() {
                frozen_ref += usize::from(matches!(reference, pivot_schema::msg::Ref::Frozen(_)));
            }
        }
    }

    for (label, count) in [
        ("multipart bodies", multipart),
        ("part content ids", part_ids),
        ("part entity headers", part_headers),
        ("part content-type parameters", part_type_params),
        ("in-dialog markers", in_dialog),
        ("confirming acks", confirming_acks),
        ("frozen non-SDP bodies", frozen_body),
        ("retransmit counts", retransmits),
        ("held automatics", held_auto),
        ("no-answer dwells", no_answer),
        ("chained attempts", chains),
        ("parallel branches", forks),
        ("blocked lanes", blocked),
        ("relay18x profiles", relay18x),
        ("prack-mode tokens", prack_mode),
        ("deviations", deviations),
        ("defect markers", defects),
        ("loopback endpoints", loopback),
        ("dedicated endpoints", dedicated),
        ("frozen tier-2 refs", frozen_ref),
        ("catalog entries", catalog),
        ("registered identities", registered),
        ("joined legs", joins),
        ("number accessors", numbered),
        ("header-targeted deviations", targeted),
        ("alt branches", alts),
        ("unordered steps", unordered),
        ("injections", injects),
        ("optional expects", optionals),
        ("background policies", background),
        ("background counters", counters),
        ("accessors", accessors),
        ("checks", checks),
        ("parallel calls", parallel_calls),
        ("stated CDR counts", cdr_counts),
        ("reasoned CDR absences", cdr_absent),
        ("capability requirements", requires),
        ("the reserved media block", media_block),
        ("relative CSeq overrides", relative_cseq),
        ("authored documents", authored),
        ("captured documents", captured),
    ] {
        assert!(count > 0, "the fixture set no longer covers {label}");
    }
    assert_eq!(
        families,
        ["fork", "prack", "refer", "reroute", "transparent"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    );
}

/// A lane verdict is a decision, not free text: the driver reads it to skip.
#[test]
fn every_lane_verdict_is_ok_or_a_reasoned_block() {
    for (name, pivot) in parsed() {
        assert!(!pivot.case.lanes.is_empty(), "{name}");
        for (lane, verdict) in &pivot.case.lanes {
            if let LaneVerdict::Blocked(reason) = verdict {
                assert!(!reason.is_empty(), "{name}: lane {lane}");
            }
        }
    }
}

/// §13: a captured document carries its provenance and its coordinates, and
/// nothing a capture could not have justified.
#[test]
fn captured_fixtures_stay_inside_the_generator_subset() {
    for (name, pivot) in parsed() {
        if pivot.case.origin != Origin::Capture {
            continue;
        }
        assert!(pivot.case.source.is_some(), "{name}");
        assert!(pivot.timing.capture_span_ms.is_some(), "{name}");
        assert!(pivot.actors.iter().all(|a| a.background.is_empty()), "{name}");
        assert!(
            pivot.flow.iter().all(|n| matches!(n, FlowNode::Message(_)) && n.after().is_empty()),
            "{name}: a captured flow is straight-line"
        );
        for step in pivot.steps() {
            assert!(step.observed.is_some(), "{name}: step {}", step.id);
            assert!(!step.optional && step.checks.is_empty(), "{name}: step {}", step.id);
        }
    }
}

/// A real clock spends the declared timeline in wall time, so every fixture
/// states a span a wall ceiling can be derived from.
#[test]
fn every_fixture_declares_a_span_a_wall_ceiling_can_be_derived_from() {
    for (name, pivot) in parsed() {
        let dwells: Vec<u64> = pivot.steps().iter().map(|step| step.delay.ms).collect();
        match pivot.timing.capture_span_ms {
            Some(measured) => assert_eq!(pivot.declared_span_ms(), measured, "{name}"),
            None => assert_eq!(pivot.declared_span_ms(), dwells.iter().sum::<u64>(), "{name}"),
        }
        assert!(
            pivot.declared_span_ms() >= dwells.iter().copied().max().unwrap_or(0),
            "{name}: a span shorter than one of its own dwells bounds nothing"
        );
    }
}

/// The exported schema PINS the version. Without it a mirror validating a v2
/// document against the v3 schema would pass it — every field v2 and v3 share
/// still validates — and only Rust would notice, which is the wrong place for a
/// generator to find out.
#[test]
fn the_exported_schema_pins_the_pivot_version() {
    let schema = serde_json::to_value(schemars::schema_for!(PivotV3)).expect("a schema");
    assert_eq!(
        schema["properties"]["pivot_version"]["const"],
        serde_json::json!(PIVOT_VERSION),
        "the schema does not pin `pivot_version`"
    );
}

/// A flow node is dispatched by hand, so the duplicate-key refusal every other
/// object gets from its derived impl has to be shown to survive that path.
#[test]
fn a_flow_node_refuses_a_duplicate_key_like_every_other_object() {
    let (_, text) = fixtures().into_iter().next().expect("a fixture");
    let with_duplicate = text.replacen("\"leg\":", "\"leg\": \"ZZZ\", \"leg\":", 1);
    let error = PivotV3::from_json(&with_duplicate).expect_err("a duplicate key is refused");
    assert!(error.to_string().contains("leg"), "{error}");
}

/// The schedule table is the fixture, byte for byte: one row per
/// `sip_retransmit::Class`, walked to its give-up.
#[test]
fn the_schedule_table_is_the_fixture_byte_for_byte() {
    let path = fixture_dir().join("schedules/table.json");
    let text = std::fs::read_to_string(&path).expect("a readable schedules/table.json");
    let printed = pivot_schema::canonical::format(&pivot_schema::schedules::schedule_table())
        .expect("the table serializes");
    assert_eq!(printed, text, "`pivot-schema schedules` drifted from tests/fixtures/schedules/table.json");
}
