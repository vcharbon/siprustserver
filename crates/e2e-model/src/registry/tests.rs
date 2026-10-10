use super::*;
// A third-party registration test binds a plain LINEAR body — the last
// in-tree use of the linear `BasicCall` now the shipped shapes are actors.
use scenario_harness::actor::scenarios::BasicCall;

#[test]
fn defaults_register_one_id_space_with_both_surfaces() {
    let reg = ShapeRegistry::with_defaults();
    // Load ids unchanged (report/metrics compatibility).
    for id in [
        "basic_call",
        "reinvite",
        "refer",
        "options_hold",
        "long_call",
        "prack_update",
        "basic_call_em",
        "reinvite_em",
        "invite_reject",
        "abandon_ringing",
        "refer_charlie_reject",
    ] {
        let d = reg.get(id).unwrap_or_else(|| panic!("load shape {id:?} missing"));
        assert!(d.load.is_some(), "{id} carries a load body");
    }
    // Functional ids unchanged; no load body.
    for id in ["basic-call", "basic-call-media", "rerouting", "transfer-refer-media"] {
        let d = reg.get(id).unwrap_or_else(|| panic!("functional shape {id:?} missing"));
        assert!(d.load.is_none(), "{id} is functional-only");
        assert!(!ShapeSpec::anchors(d).is_empty(), "{id} publishes anchors");
    }
    // The dual-body shape carries BOTH the anchor vocabulary and a load body.
    let dual = reg.get("rerouting_prack").expect("dual-body shape registered");
    assert!(dual.load.is_some() && !dual.anchors.is_empty());
    assert!(dual.needs_bob2, "rerouting needs the second callee receiver");
}

#[test]
fn default_and_abnormal_end_mixes_preserve_canonical_weights_and_add_phase_c() {
    let reg = ShapeRegistry::with_defaults();
    let mix: std::collections::BTreeMap<&str, f64> =
        reg.default_mix().iter().map(|d| (d.id, d.default_weight.unwrap())).collect();
    // The canonical weights (report/metrics compat).
    assert_eq!(mix.get("basic_call"), Some(&4.0));
    assert_eq!(mix.get("reinvite"), Some(&2.0));
    assert_eq!(mix.get("options_hold"), Some(&1.0));
    assert_eq!(mix.get("refer"), Some(&1.0));
    // The catalog shapes carry their catalog weights.
    for (id, w) in [
        ("crossing_bye", 1.0),
        ("forked", 1.0),
        ("forked_reliable", 0.5),
        ("cancel_answer_crossing", 0.5),
        ("prack_update_early", 0.5),
        ("reinvite10", 0.5),
    ] {
        assert_eq!(mix.get(id), Some(&w), "{id} weight");
    }
    // The generated cross-product cells are id-addressable ONLY (no weight),
    // so the default mix stays a representative sample (no bob2 in the mix).
    assert!(!mix.contains_key("reroute+reinvite"), "generated cells are not in the default mix");
    assert!(reg.get("reroute+reinvite").is_some(), "but they ARE registered by id");

    let abnormal: Vec<&str> = reg.abnormal_end_mix().iter().map(|d| d.id).collect();
    assert_eq!(abnormal, vec!["abandon_ringing", "invite_reject", "refer_charlie_reject"]);
}

#[test]
fn load_scenario_constructs_from_inputs() {
    let reg = ShapeRegistry::with_defaults();
    let inputs = ScenarioInputs { refer_key: "k-42".into() };
    assert_eq!(reg.load_scenario("basic_call", &inputs).unwrap().id(), "basic_call");
    assert_eq!(reg.load_scenario("refer", &inputs).unwrap().id(), "refer");
    assert!(reg.load_scenario("basic-call", &inputs).is_none(), "functional-only: no body");
    assert!(reg.load_scenario("nope", &inputs).is_none());
}

/// The boolean sugar expands to the default label list (role ==
/// single prefix == label, bob → bob2 → charlie bind order), so every
/// pre-legs shape drives the wire byte-identically.
#[test]
fn callee_legs_boolean_sugar_expands_to_the_historic_labels() {
    let flat = |legs: &[LegSpec]| -> Vec<(&str, Vec<&str>)> {
        legs.iter().map(|l| (l.role, l.ruri_prefixes.to_vec())).collect()
    };
    let with = |d: ShapeDescriptor| flat(d.callee_legs());

    assert_eq!(with(ShapeDescriptor::new("s")), vec![("bob", vec!["bob"])]);
    assert_eq!(
        with(ShapeDescriptor::new("s").needs_bob2()),
        vec![("bob", vec!["bob"]), ("bob2", vec!["bob2"])]
    );
    assert_eq!(
        with(ShapeDescriptor::new("s").needs_charlie()),
        vec![("bob", vec!["bob"]), ("charlie", vec!["charlie"])]
    );
    assert_eq!(
        with(ShapeDescriptor::new("s").needs_bob2().needs_charlie()),
        vec![("bob", vec!["bob"]), ("bob2", vec!["bob2"]), ("charlie", vec!["charlie"])]
    );
}

/// An explicit `legs` declaration wins over the boolean sugar — the open
/// form a third-party shape uses: roles addressed on the wire by
/// dialled-digit prefixes, multiple prefixes per leg.
#[test]
fn explicit_legs_override_the_boolean_sugar() {
    const OPEN_LEGS: &[LegSpec] = &[
        LegSpec { role: "bob", ruri_prefixes: &["+1555", "9005"] },
        LegSpec { role: "mrf", ruri_prefixes: &["9007"] },
    ];
    let d = ShapeDescriptor::new("open_mrf_rbt").legs(OPEN_LEGS).needs_charlie();
    assert_eq!(d.callee_legs(), OPEN_LEGS, "declared legs win; the sugar is ignored");
}

#[test]
fn third_party_registration_is_open() {
    let mut reg = ShapeRegistry::with_defaults();
    reg.register(
        ShapeDescriptor::new("vendor_flow").anchors(CALL_ANCHORS).load_shared(Arc::new(BasicCall)),
    );
    assert!(reg.get("vendor_flow").is_some());
    assert!(reg.ids().contains(&"vendor_flow".to_string()));
}

#[test]
#[should_panic(expected = "duplicate shape id")]
fn duplicate_id_panics() {
    let mut reg = ShapeRegistry::with_defaults();
    reg.register(ShapeDescriptor::new("basic_call"));
}
