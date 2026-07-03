//! The committed example Load profile (`e2e/loadprofiles/endurance-baseline.json`)
//! must load, validate, and name only real shapes — so a shipped baseline never
//! rots (the same load-time guarantee `validate_case` gives Test cases).

use std::path::PathBuf;

use e2e_model::{load_load_profile, ShapeRegistry};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

#[test]
fn committed_endurance_baseline_loads_and_validates() {
    let path = workspace_root().join("e2e/loadprofiles/endurance-baseline.json");
    let profile =
        load_load_profile(&path).unwrap_or_else(|e| panic!("committed example profile: {e}"));
    // `load` already validates; re-assert the salient shape.
    profile.validate().expect("committed example is valid");
    assert!(profile.cps > 0.0);
    assert!(!profile.mix.is_empty(), "the baseline ships a mix");

    // Every mix entry names a shape the registry actually serves as a load body
    // (a typo'd shape id in the committed baseline would fail the run at startup).
    let registry = ShapeRegistry::with_defaults();
    for entry in &profile.mix {
        let d = registry
            .get(&entry.shape)
            .unwrap_or_else(|| panic!("baseline mix names unknown shape {:?}", entry.shape));
        assert!(
            d.load.is_some(),
            "baseline mix shape {:?} has no load body",
            entry.shape
        );
    }
}
