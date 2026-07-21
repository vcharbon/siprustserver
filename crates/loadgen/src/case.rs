//! Test-case-driven load calls — the **parameters axis** on the load surface.
//!
//! A [`LoadCase`] attaches an authored `e2e-model` Test case (with its optional
//! binding pool, `e2e_model::bindings`) to a scenario mix entry. Per call the
//! driver resolves ONE binding — pool walk per mode, tokens expanded — and:
//!
//! - the resolved **core** From/To/R-URI rides the egress `outgoing_invite`
//!   path ([`scenario_harness::realcall::CallEnv::outgoing_invite`], the same
//!   fold-in as e2e-core's `InfraRuntime::outgoing_invite`);
//! - the **recognized extras** become per-call dwells overriding the global
//!   CLI defaults (`ring_delay_ms`, `talk_time_ms`, `reinvite_gap_ms`,
//!   `long_hold_secs`, `options_cadence_ms`) — killing the "dwells are global"
//!   limitation while [`CallConfig`](crate::driver::CallConfig) keeps the
//!   global defaults;
//! - a one-line **banner** (the actual From/To used + the case/seq provenance)
//!   is threaded into the sampled callflow page header. Prometheus/bucket
//!   labels stay scenario-keyed — a binding never becomes label cardinality;
//! - the case's **checks** (inline blocks + referenced Check sets) are
//!   evaluated over a SAMPLED call's recorded trace by the e2e check engine
//!   ([`e2e_model::checks`], the one shared path), failing checks reclassify
//!   the call to `check_fail`; its **allowViolations** exempt the named RFC
//!   audit rules per call.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use e2e_model::checks::Bindings;
use e2e_model::model::{CheckSet, Input, TestCase, load_test_case};
use e2e_model::{BindingResolver, CheckVerdict, ResolvedBinding};
use scenario_harness::realcall::CoreIdentity;
use scenario_harness::{AnchorTag, WaiverScope};
use sip_net::RecordedSipEntry;

/// The recognized per-call dwell overrides carried in a resolved Input's
/// `extras`. `None` = keep the run's global default for that knob. Values may
/// be authored as JSON numbers or as strings (so they can carry expansion
/// tokens); anything else — or a recognized key that does not parse to a
/// non-negative integer — panics loudly (a typo must never silently fall back
/// to the global default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DwellOverrides {
    pub ring_delay: Option<Duration>,
    pub talk_time: Option<Duration>,
    pub reinvite_gap: Option<Duration>,
    pub long_hold: Option<Duration>,
    pub options_cadence: Option<Duration>,
}

impl DwellOverrides {
    /// Extract the recognized dwell keys from a resolved input's extras.
    /// Unrecognized extras are left alone — they are the open per-shape
    /// parameter map, not dwells.
    pub fn from_extras(input: &Input) -> Self {
        let ms = |key: &str| parse_u64(input, key).map(Duration::from_millis);
        DwellOverrides {
            ring_delay: ms("ring_delay_ms"),
            talk_time: ms("talk_time_ms"),
            reinvite_gap: ms("reinvite_gap_ms"),
            long_hold: parse_u64(input, "long_hold_secs").map(Duration::from_secs),
            options_cadence: ms("options_cadence_ms"),
        }
    }
}

fn parse_u64(input: &Input, key: &str) -> Option<u64> {
    let v = input.extras.get(key)?;
    let parsed = match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    Some(parsed.unwrap_or_else(|| {
        panic!("case extras {key:?} must be a non-negative integer (ms/secs), got {v}")
    }))
}

/// One per-call resolution of a [`LoadCase`]: the core identity to fold into
/// the outgoing INVITE, the dwell overrides, the banner line for the sampled
/// callflow header, and the fully resolved (pool-expanded) [`Input`] — the
/// `${input.*}` binding source when the case's checks run over the call.
#[derive(Debug, Clone)]
pub struct ResolvedCall {
    pub core: CoreIdentity,
    pub dwells: DwellOverrides,
    /// `case=<id> seq=<n> [entry=<i>] from=… to=… ruri=…` — shown in the
    /// sampled page banner; never a metrics label.
    pub banner: String,
    /// THIS call's resolved input (base overlaid by the walked pool entry,
    /// tokens expanded) — what `${input.<field>}` in a check value binds to.
    pub input: Input,
}

/// A Test case attached to a scenario mix entry, with its per-run
/// [`BindingResolver`] (`Send + Sync`; shared via `Arc` across the call fleet)
/// and its check surface (blocks + referenced Check sets + audit waivers).
pub struct LoadCase {
    case: TestCase,
    /// The run's Check-set store (`--check-sets-dir`); referenced ids were
    /// validated present at construction.
    check_sets: BTreeMap<String, CheckSet>,
    /// The case's `allowViolations`, LOWERED to structural [`WaiverScope`]s
    /// (ADR-0024 §6): rule-only + `.conditional()`, so filtering matches on the
    /// rule name alone AND an author's defensive list never trips the
    /// per-campaign unused-waiver gate (like the functional lane's
    /// `Harness::allow_violation`).
    waivers: Vec<WaiverScope>,
    resolver: BindingResolver,
}

impl LoadCase {
    /// Load an authored Test-case JSON and build its per-run resolver. The
    /// binding pool + expansion tokens AND the check surface (referenced
    /// Check-set ids, selector form, canonical anchors, op/value coherence) are
    /// validated here — the shape-independent half of `validate_case` (loadgen
    /// picks its scenario by the `--scenario` flag, not `compatibleShapes`).
    /// Panics on any problem — a bad case must fail at startup, not mid-run.
    pub fn load(path: &Path, check_sets: &BTreeMap<String, CheckSet>, seed: u64) -> Self {
        let case =
            load_test_case(path).unwrap_or_else(|e| panic!("--case {}: {e}", path.display()));
        Self::new(case, check_sets, seed).unwrap_or_else(|problems| {
            panic!("--case {}: invalid case:\n  - {}", path.display(), problems.join("\n  - "))
        })
    }

    /// Build from an in-memory [`TestCase`] (the loader-free seam for tests).
    /// `Err` lists every problem: binding problems (empty pool, malformed
    /// tokens) plus the shape-independent check lints
    /// ([`e2e_model::collect_case_blocks`]) — the same validations
    /// `validate_case` runs at e2e load time, minus the shape-compat half.
    pub fn new(
        case: TestCase,
        check_sets: &BTreeMap<String, CheckSet>,
        seed: u64,
    ) -> Result<Self, Vec<String>> {
        let mut problems =
            e2e_model::bindings::validate_bindings(&case.input, case.bindings.as_ref());
        let (_, check_problems) = e2e_model::collect_case_blocks(&case, check_sets);
        problems.extend(check_problems);
        if !problems.is_empty() {
            return Err(problems);
        }
        let resolver = BindingResolver::new(case.input.clone(), case.bindings.clone(), seed);
        Ok(Self {
            waivers: case
                .allow_violations
                .iter()
                .map(|rule| {
                    WaiverScope::rule(rule.clone(), "case allowViolations").conditional()
                })
                .collect(),
            // Keep only the sets the case references (bounded per-case store).
            check_sets: case
                .check_sets
                .iter()
                .filter_map(|id| check_sets.get(id).map(|s| (id.clone(), s.clone())))
                .collect(),
            case,
            resolver,
        })
    }

    /// The case id (banner provenance).
    pub fn id(&self) -> &str {
        &self.case.id
    }

    /// The structural RFC-audit waivers this case declares (its `allowViolations`
    /// lowered to rule-only conditional [`WaiverScope`]s).
    pub fn waivers(&self) -> &[WaiverScope] {
        &self.waivers
    }

    /// Does this case bind any checks (inline or via Check sets)?
    pub fn has_checks(&self) -> bool {
        !self.case.checks.is_empty() || !self.case.check_sets.is_empty()
    }

    /// Evaluate the case's checks over a SAMPLED call's recorded trace — the
    /// SAME engine the e2e executor runs ([`e2e_model::checks::evaluate_case_over`]),
    /// fed the load surface's halves: the per-call binder's recorded entries,
    /// the `CallCtx`-collected anchors, this call's resolved input and the
    /// run's SUT ingress as `${infra.lbVip}`.
    pub fn evaluate(
        &self,
        entries: &[RecordedSipEntry],
        anchors: &[AnchorTag],
        input: &Input,
        lb_vip: SocketAddr,
    ) -> Vec<CheckVerdict> {
        e2e_model::checks::evaluate_case_over(
            &self.case,
            &self.check_sets,
            entries,
            anchors,
            &Bindings { input, lb_vip },
        )
    }

    /// Resolve the binding for ONE call.
    pub fn resolve(&self) -> ResolvedCall {
        let ResolvedBinding { input, seq, entry } = self.resolver.resolve();
        let core = CoreIdentity {
            from: input.core.from.clone(),
            to: input.core.to.clone(),
            ruri: input.core.ruri.clone(),
        };
        let dwells = DwellOverrides::from_extras(&input);
        let entry_part = entry.map(|i| format!(" entry={i}")).unwrap_or_default();
        let identity = core.summary().map(|s| format!(" {s}")).unwrap_or_default();
        let banner = format!("binding: case={} seq={seq}{entry_part}{identity}", self.case.id);
        ResolvedCall { core, dwells, banner, input }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use e2e_model::{BindingMode, BindingPool};
    use e2e_model::shape::CoreInput;

    fn case_with(input: Input, bindings: Option<BindingPool>) -> TestCase {
        TestCase {
            schema: None,
            id: "t".into(),
            description: None,
            compatible_shapes: vec!["basic-call".into()],
            input,
            bindings,
            check_sets: vec![],
            checks: vec![],
            allow_violations: vec![],
        }
    }

    fn no_sets() -> BTreeMap<String, CheckSet> {
        BTreeMap::new()
    }

    #[test]
    fn dwell_extras_override_and_unknown_extras_are_ignored() {
        let mut input = Input::default();
        input.extras.insert("ring_delay_ms".into(), serde_json::json!(25));
        input.extras.insert("long_hold_secs".into(), serde_json::json!("3"));
        input.extras.insert("some_shape_param".into(), serde_json::json!("free-form"));
        let lc = LoadCase::new(case_with(input, None), &no_sets(), 1).unwrap();
        let r = lc.resolve();
        assert_eq!(r.dwells.ring_delay, Some(Duration::from_millis(25)));
        assert_eq!(r.dwells.long_hold, Some(Duration::from_secs(3)));
        assert_eq!(r.dwells.talk_time, None, "unset knob keeps the global default");
    }

    #[test]
    fn resolve_yields_expanding_identities_and_a_banner() {
        let entry = Input {
            core: CoreInput { from: Some("sip:+331${seq:4}@pool".into()), ..Default::default() },
            extras: Default::default(),
        };
        let pool = BindingPool { mode: BindingMode::Seq, entries: vec![entry] };
        let lc = LoadCase::new(case_with(Input::default(), Some(pool)), &no_sets(), 1).unwrap();
        let a = lc.resolve();
        let b = lc.resolve();
        assert_eq!(a.core.from.as_deref(), Some("sip:+3310000@pool"));
        assert_eq!(b.core.from.as_deref(), Some("sip:+3310001@pool"));
        assert!(a.banner.contains("case=t"), "{}", a.banner);
        assert!(a.banner.contains("from=sip:+3310000@pool"), "{}", a.banner);
    }

    #[test]
    fn bad_tokens_and_empty_pool_fail_construction() {
        let bad = Input {
            core: CoreInput { from: Some("sip:${bogus}@x".into()), ..Default::default() },
            extras: Default::default(),
        };
        assert!(LoadCase::new(case_with(bad, None), &no_sets(), 1).is_err());
        let empty = BindingPool { mode: BindingMode::Seq, entries: vec![] };
        assert!(LoadCase::new(case_with(Input::default(), Some(empty)), &no_sets(), 1).is_err());
    }

    #[test]
    #[should_panic(expected = "ring_delay_ms")]
    fn a_malformed_dwell_value_panics_instead_of_silently_defaulting() {
        let mut input = Input::default();
        input.extras.insert("ring_delay_ms".into(), serde_json::json!("not-a-number"));
        let lc = LoadCase::new(case_with(input, None), &no_sets(), 1).unwrap();
        let _ = lc.resolve();
    }
}
