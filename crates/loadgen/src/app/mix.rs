//! The run's weighted shape mix: `--scenario` specs, else the profile's mix,
//! else the registry's default mix, each entry with its attached Test case and
//! its per-shape loss/retransmit tuning.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use e2e_model::LoadProfile;

use super::Args;
use crate::case::CaseLoader;
use crate::{CallTuning, Correlation, LoadCase, MixEntry, ScenarioInputs, ShapeRegistry};

/// The resolved mix: its entries and their per-shape tuning overrides.
pub(super) struct Mix {
    pub(super) entries: Vec<MixEntry>,
    pub(super) tuning: HashMap<String, CallTuning>,
}

/// Resolve the mix: an explicit `--scenario` set replaces the profile's whole
/// mix; with neither, the registry's default mix runs under the global tuning.
/// The global `--case` attaches to every entry without its own.
pub(super) fn resolve_mix(
    args: &Args,
    profile: &LoadProfile,
    registry: &ShapeRegistry,
    inputs: &ScenarioInputs,
    base: CallTuning,
    cases: &mut CaseLoader<'_>,
) -> Mix {
    // ONE shared resolver for the global case, so its `${seq}` counter is
    // monotone across the whole run.
    let global_case: Option<Arc<LoadCase>> = args.case.as_deref().map(|p| cases.load(p));
    let with_global = |entry: MixEntry| match entry.case.is_some() {
        true => entry,
        false => entry.with_case(global_case.clone()),
    };
    let mut tuning = HashMap::new();
    let entries = if !args.scenarios.is_empty() {
        let mut entries = Vec::new();
        for spec in &args.scenarios {
            let (entry, t) = parse_scenario_spec(spec, base, registry, inputs, cases);
            tuning.insert(entry.id.to_string(), t);
            entries.push(with_global(entry));
        }
        entries
    } else if !profile.mix.is_empty() {
        let mut entries = Vec::new();
        for m in &profile.mix {
            let (entry, t) = resolve_profile_mix(m, base, registry, inputs, cases);
            tuning.insert(entry.id.to_string(), t);
            entries.push(with_global(entry));
        }
        entries
    } else {
        MixEntry::default_mix(registry, inputs).into_iter().map(with_global).collect()
    };
    Mix { entries, tuning }
}

/// From-user correlation takes each call's key from its attached case's
/// resolved caller, so every mix entry must carry a case. `Err` names the
/// entries without one.
pub fn check_from_user_mix(correlation: &Correlation, mix: &[MixEntry]) -> Result<(), String> {
    if !correlation.is_from_user() {
        return Ok(());
    }
    let caseless: Vec<&str> = mix.iter().filter(|e| e.case.is_none()).map(|e| e.id).collect();
    if caseless.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "--correlate from-user needs a case (--case or case=) on every mix entry; \
             none on {caseless:?}"
        ))
    }
}

/// Parse one `--scenario` spec
/// (`name[=weight][,drop=<f>][,retransmit[=<bool>]][,case=<path.json>]`)
/// into its resolved [`MixEntry`] (the shape looked up in the unified
/// `registry`, its load body constructed from the per-run `inputs`) plus the
/// per-scenario [`CallTuning`] (starting from `base` and overridden by the
/// trailing tokens); the Test case attached with `case=` comes from `cases`.
fn parse_scenario_spec(
    spec: &str,
    base: CallTuning,
    registry: &ShapeRegistry,
    inputs: &ScenarioInputs,
    cases: &mut CaseLoader<'_>,
) -> (MixEntry, CallTuning) {
    let mut parts = spec.split(',');
    let head = parts.next().unwrap_or(spec);
    let (name, weight) = head
        .split_once('=')
        .map(|(n, w)| (n, w.parse::<f64>().unwrap_or(1.0)))
        .unwrap_or((head, 1.0));
    let mut entry = MixEntry::by_id(registry, name, inputs, weight).unwrap_or_else(|| {
        panic!(
            "unknown load scenario {name:?} (known: {:?})",
            registry.iter().filter(|d| d.load.is_some()).map(|d| d.id).collect::<Vec<_>>()
        )
    });
    let mut t = base;
    for tok in parts {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        match tok.split_once('=') {
            Some(("drop", v)) => {
                t.drop_rate = v.parse().unwrap_or_else(|_| panic!("bad drop rate in {spec:?}"))
            }
            Some(("retransmit", v)) => {
                t.retransmit =
                    v.parse().unwrap_or_else(|_| panic!("bad retransmit bool in {spec:?}"))
            }
            Some(("case", path)) => {
                entry.case = Some(cases.load(Path::new(path)));
            }
            None if tok == "retransmit" => t.retransmit = true,
            None if tok == "drop" => {
                if t.drop_rate <= 0.0 {
                    t.drop_rate = 0.001
                }
            }
            _ => panic!("unknown scenario tuning token {tok:?} in {spec:?}"),
        }
    }
    (entry, t)
}

/// Resolve one [`e2e_model::MixSpec`] (a profile mix entry) into its [`MixEntry`]
/// paired with its per-scenario [`CallTuning`], mirroring `parse_scenario_spec`
/// for the CLI scenario string form: the shape is looked up by id, carries its
/// weight and an attached `case`, with per-scenario `dropRate`/`retransmit`
/// overriding `base`.
pub(super) fn resolve_profile_mix(
    m: &e2e_model::MixSpec,
    base: CallTuning,
    registry: &ShapeRegistry,
    inputs: &ScenarioInputs,
    cases: &mut CaseLoader<'_>,
) -> (MixEntry, CallTuning) {
    let mut entry = MixEntry::by_id(registry, &m.shape, inputs, m.weight).unwrap_or_else(|| {
        panic!(
            "load-profile mix references unknown shape {:?} (known: {:?})",
            m.shape,
            registry.iter().filter(|d| d.load.is_some()).map(|d| d.id).collect::<Vec<_>>()
        )
    });
    if let Some(path) = &m.case {
        entry.case = Some(cases.load(path));
    }
    let t = CallTuning {
        drop_rate: m.drop_rate.unwrap_or(base.drop_rate),
        retransmit: m.retransmit.unwrap_or(base.retransmit),
        drop_nth: None,
    };
    (entry, t)
}
