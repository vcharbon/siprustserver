//! The **parameters axis** of the fused model (loadgen⇄e2e): a Test case's
//! optional [`BindingPool`] — a walkable pool of [`Input`] overlays whose string
//! fields may embed per-call **expansion tokens** — plus the per-run
//! [`BindingResolver`] that turns "one Test case" into "a different identity per
//! call".
//!
//! Tokens (resolved once per call, over the *merged* base⊕entry input):
//!
//! - `${seq}`    — the monotone per-run call counter (starts at 0);
//! - `${seq:N}`  — the counter zero-padded / truncated to its **last N digits**
//!   (`7` → `0007`, `123456` → `3456` for N = 4);
//! - `${rand:N}` — N random digits (fresh per occurrence, from the resolver's
//!   deterministic seeded RNG).
//!
//! **Wrap-allowed semantics**: the pool is walked sequentially (`seq` mode:
//! entry `seq % len`) or by seeded random pick (`random` mode); once the pool
//! wraps, identities repeat — that is fine by design (a load run dials a finite
//! subscriber pool, not an infinite one). Combined with `${seq}` the repeats
//! stay distinguishable when the author wants them to be.
//!
//! Absent `bindings` = single-Input behaviour: the resolver expands the case's
//! base input alone, so a case without a pool is byte-for-byte the historic
//! one-identity case (tokens still expand, if authored).
//!
//! Malformed tokens (`${seq:}`, `${bogus}`, an unclosed `${…`) are a LOUD
//! failure: [`validate_bindings`] reports them at load time and the expander
//! panics if one slips through — never a silent literal fall-through that masks
//! a typo (the harness philosophy).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::Input;
use crate::shape::CoreInput;

/// How the pool is walked call-to-call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BindingMode {
    /// Sequential walk: call *n* uses entry `n % entries.len()` (wraps).
    #[default]
    Seq,
    /// Random walk: each call picks a uniformly random entry (seeded,
    /// deterministic per run; repeats freely).
    Random,
}

/// The authored `bindings` field of a Test case: a pool of [`Input`] overlays.
/// Each call resolves ONE entry (per [`BindingMode`]), merges it over the case's
/// base `input` (entry fields win; extras merge key-by-key), then expands the
/// tokens — see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BindingPool {
    #[serde(default)]
    pub mode: BindingMode,
    /// The Input overlays (core From/To/R-URI + extras). Must be non-empty.
    pub entries: Vec<Input>,
}

// ---------------------------------------------------------------------------
// Token expansion
// ---------------------------------------------------------------------------

/// Expand every `${…}` token in `s`. `seq` is the per-call counter;
/// `rand_digits(n)` yields `n` fresh random digits per occurrence.
/// `Err` on a malformed/unknown token (see [`lint_tokens`]).
fn expand_str(
    s: &str,
    seq: u64,
    rand_digits: &mut dyn FnMut(usize) -> String,
) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("unclosed token `${{` in {s:?}"));
        };
        let token = &after[..end];
        match parse_token(token)? {
            Token::Seq => out.push_str(&seq.to_string()),
            Token::SeqDigits(n) => out.push_str(&last_n_digits(seq, n)),
            Token::RandDigits(n) => out.push_str(&rand_digits(n)),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

enum Token {
    Seq,
    SeqDigits(usize),
    RandDigits(usize),
}

fn parse_token(token: &str) -> Result<Token, String> {
    match token.split_once(':') {
        None if token == "seq" => Ok(Token::Seq),
        Some(("seq", n)) => Ok(Token::SeqDigits(parse_width("seq", n)?)),
        Some(("rand", n)) => Ok(Token::RandDigits(parse_width("rand", n)?)),
        _ => Err(format!(
            "unknown expansion token `${{{token}}}` (expected ${{seq}}, ${{seq:N}} or ${{rand:N}})"
        )),
    }
}

fn parse_width(kind: &str, n: &str) -> Result<usize, String> {
    n.parse::<usize>()
        .ok()
        .filter(|w| (1..=64).contains(w))
        .ok_or_else(|| format!("bad digit count in `${{{kind}:{n}}}` (expected 1..=64)"))
}

/// `seq` zero-padded / truncated to its LAST `n` digits (`7`,4 → `0007`;
/// `123456`,4 → `3456`).
fn last_n_digits(seq: u64, n: usize) -> String {
    let s = format!("{seq:0>width$}", width = n);
    s[s.len() - n..].to_string()
}

/// Walk every string field of `input` (core From/To/R-URI + extras values,
/// recursively through objects/arrays) and apply `f`. The one traversal shared
/// by expansion and the load-time token lint.
fn visit_strings(input: &Input, f: &mut dyn FnMut(&str) -> Result<(), String>) -> Result<(), String> {
    for s in [&input.core.from, &input.core.to, &input.core.ruri].into_iter().flatten() {
        f(s)?;
    }
    fn visit_value(v: &serde_json::Value, f: &mut dyn FnMut(&str) -> Result<(), String>) -> Result<(), String> {
        match v {
            serde_json::Value::String(s) => f(s),
            serde_json::Value::Array(items) => items.iter().try_for_each(|i| visit_value(i, f)),
            serde_json::Value::Object(map) => map.values().try_for_each(|i| visit_value(i, f)),
            _ => Ok(()),
        }
    }
    input.extras.values().try_for_each(|v| visit_value(v, f))
}

/// Load-time token lint over one [`Input`]: every `${…}` in every string field
/// (core AND extras, recursively) must parse. Returns the problems (empty =
/// clean). Fed into `validate_case` so a typo'd token fails at load, not
/// mid-run.
pub fn lint_tokens(input: &Input) -> Vec<String> {
    let mut problems = Vec::new();
    let mut lint = |s: &str| -> Result<(), String> {
        // Dry-run the expander (constant rand) — one grammar, one parser.
        if let Err(e) = expand_str(s, 0, &mut |n| "0".repeat(n)) {
            problems.push(e);
        }
        Ok(())
    };
    let _ = visit_strings(input, &mut lint);
    problems
}

/// The **shape-independent** binding validation shared by `validate_case`
/// (the e2e load-time gate) and the loadgen CLI (`LoadCase`): base-input token
/// lint, pool non-emptiness, per-entry token lint. Returns raw problems (the
/// caller prefixes the case id).
pub fn validate_bindings(input: &Input, pool: Option<&BindingPool>) -> Vec<String> {
    let mut problems: Vec<String> =
        lint_tokens(input).into_iter().map(|p| format!("input: {p}")).collect();
    if let Some(pool) = pool {
        if pool.entries.is_empty() {
            problems.push("bindings.entries is empty".to_string());
        }
        for (i, entry) in pool.entries.iter().enumerate() {
            problems.extend(
                lint_tokens(entry).into_iter().map(|p| format!("bindings.entries[{i}]: {p}")),
            );
        }
    }
    problems
}

/// Expand every string field of `input` in place (core + extras, recursively).
/// Panics on a malformed token — [`lint_tokens`] catches those at load time.
fn expand_input(input: &mut Input, seq: u64, rand_digits: &mut dyn FnMut(usize) -> String) {
    let expand = |s: &mut String, rand_digits: &mut dyn FnMut(usize) -> String| {
        *s = expand_str(s, seq, rand_digits)
            .unwrap_or_else(|e| panic!("binding expansion failed: {e}"));
    };
    for field in [&mut input.core.from, &mut input.core.to, &mut input.core.ruri]
        .into_iter()
        .flatten()
    {
        expand(field, rand_digits);
    }
    fn expand_value(v: &mut serde_json::Value, seq: u64, rand_digits: &mut dyn FnMut(usize) -> String) {
        match v {
            serde_json::Value::String(s) => {
                *s = expand_str(s, seq, rand_digits)
                    .unwrap_or_else(|e| panic!("binding expansion failed: {e}"));
            }
            serde_json::Value::Array(items) => {
                items.iter_mut().for_each(|i| expand_value(i, seq, rand_digits))
            }
            serde_json::Value::Object(map) => {
                map.values_mut().for_each(|i| expand_value(i, seq, rand_digits))
            }
            _ => {}
        }
    }
    input.extras.values_mut().for_each(|v| expand_value(v, seq, rand_digits));
}

/// Merge one pool entry OVER the base input: a set core field on the entry wins;
/// entry extras merge key-by-key over the base extras (entry value wins).
fn overlay(base: &Input, entry: &Input) -> Input {
    let mut merged = Input {
        core: CoreInput {
            from: entry.core.from.clone().or_else(|| base.core.from.clone()),
            to: entry.core.to.clone().or_else(|| base.core.to.clone()),
            ruri: entry.core.ruri.clone().or_else(|| base.core.ruri.clone()),
        },
        extras: base.extras.clone(),
    };
    for (k, v) in &entry.extras {
        merged.extras.insert(k.clone(), v.clone());
    }
    merged
}

// ---------------------------------------------------------------------------
// The per-run resolver
// ---------------------------------------------------------------------------

/// One resolved per-call binding: the fully merged + expanded [`Input`], plus
/// where it came from (for the sampled-callflow banner — the Prometheus labels
/// deliberately do NOT carry it, so pool size never becomes label cardinality).
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    /// The merged (base ⊕ entry) input with every token expanded — the actual
    /// From/To/R-URI + extras this call uses.
    pub input: Input,
    /// The per-run call counter this resolution consumed (the `${seq}` value).
    pub seq: u64,
    /// The pool entry index used (`None` = no pool; base input only).
    pub entry: Option<usize>,
}

/// The per-run, `Send + Sync` binding resolver for one Test case: a monotone
/// call counter + a seeded RNG (deterministic per seed — reproducible runs,
/// injectable in tests). Cheap to call from thousands of concurrent per-call
/// tasks (two atomics-and-a-mutex, no allocation beyond the resolved Input).
pub struct BindingResolver {
    base: Input,
    pool: Option<BindingPool>,
    seq: AtomicU64,
    rng: Mutex<u64>,
}

impl BindingResolver {
    /// Build a resolver over a case's base `input` + optional `bindings` pool.
    /// `seed` drives BOTH the `random`-mode pool walk and `${rand:N}` digits.
    pub fn new(base: Input, pool: Option<BindingPool>, seed: u64) -> Self {
        if let Some(p) = &pool {
            assert!(!p.entries.is_empty(), "binding pool must have at least one entry");
        }
        Self { base, pool, seq: AtomicU64::new(0), rng: Mutex::new(seed.max(1)) }
    }

    /// Resolve the binding for ONE call: bump the counter, walk the pool (per
    /// mode, wrap-allowed), merge entry-over-base, expand tokens.
    pub fn resolve(&self) -> ResolvedBinding {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (mut input, entry) = match &self.pool {
            None => (self.base.clone(), None),
            Some(pool) => {
                let idx = match pool.mode {
                    BindingMode::Seq => (seq % pool.entries.len() as u64) as usize,
                    BindingMode::Random => (self.next_rand() % pool.entries.len() as u64) as usize,
                };
                (overlay(&self.base, &pool.entries[idx]), Some(idx))
            }
        };
        let mut rand_digits = |n: usize| {
            (0..n).map(|_| char::from(b'0' + (self.next_rand() % 10) as u8)).collect::<String>()
        };
        expand_input(&mut input, seq, &mut rand_digits);
        ResolvedBinding { input, seq, entry }
    }

    fn next_rand(&self) -> u64 {
        let mut state = self.rng.lock().unwrap();
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn input(from: Option<&str>, to: Option<&str>, ruri: Option<&str>) -> Input {
        Input {
            core: CoreInput {
                from: from.map(str::to_string),
                to: to.map(str::to_string),
                ruri: ruri.map(str::to_string),
            },
            extras: Default::default(),
        }
    }

    #[test]
    fn seq_token_expands_the_monotone_counter() {
        let r = BindingResolver::new(input(Some("sip:u${seq}@x"), None, None), None, 7);
        assert_eq!(r.resolve().input.core.from.as_deref(), Some("sip:u0@x"));
        assert_eq!(r.resolve().input.core.from.as_deref(), Some("sip:u1@x"));
        assert_eq!(r.resolve().input.core.from.as_deref(), Some("sip:u2@x"));
    }

    #[test]
    fn seq_n_zero_pads_and_truncates_to_last_n_digits() {
        assert_eq!(last_n_digits(7, 4), "0007");
        assert_eq!(last_n_digits(123456, 4), "3456");
        assert_eq!(last_n_digits(0, 1), "0");
        // End-to-end through the expander.
        let out = expand_str("+331${seq:4}", 42, &mut |_| unreachable!()).unwrap();
        assert_eq!(out, "+3310042");
    }

    #[test]
    fn rand_n_is_deterministic_per_seed_and_fresh_per_occurrence() {
        let a = BindingResolver::new(input(Some("${rand:6}-${rand:6}"), None, None), None, 0xACE);
        let b = BindingResolver::new(input(Some("${rand:6}-${rand:6}"), None, None), None, 0xACE);
        let ra = a.resolve().input.core.from.unwrap();
        let rb = b.resolve().input.core.from.unwrap();
        assert_eq!(ra, rb, "same seed → same digits");
        let (left, right) = ra.split_once('-').unwrap();
        assert_eq!(left.len(), 6);
        assert!(left.chars().all(|c| c.is_ascii_digit()), "{left:?}");
        assert_ne!(left, right, "each occurrence draws fresh digits");
        // A different seed diverges.
        let c = BindingResolver::new(input(Some("${rand:6}-${rand:6}"), None, None), None, 0xBEE);
        assert_ne!(c.resolve().input.core.from.unwrap(), ra);
    }

    #[test]
    fn seq_mode_walks_the_pool_in_order_and_wraps() {
        let pool = BindingPool {
            mode: BindingMode::Seq,
            entries: vec![
                input(Some("sip:a@x"), None, None),
                input(Some("sip:b@x"), None, None),
            ],
        };
        let r = BindingResolver::new(Input::default(), Some(pool), 1);
        let picks: Vec<_> =
            (0..5).map(|_| r.resolve().input.core.from.unwrap()).collect();
        // Wrap-allowed by design: identities repeat after the pool wraps.
        assert_eq!(picks, ["sip:a@x", "sip:b@x", "sip:a@x", "sip:b@x", "sip:a@x"]);
    }

    #[test]
    fn random_mode_is_seed_deterministic_and_covers_the_pool() {
        let pool = || BindingPool {
            mode: BindingMode::Random,
            entries: (0..4).map(|i| input(Some(&format!("sip:e{i}@x")), None, None)).collect(),
        };
        let a = BindingResolver::new(Input::default(), Some(pool()), 0x5EED);
        let b = BindingResolver::new(Input::default(), Some(pool()), 0x5EED);
        let wa: Vec<_> = (0..16).map(|_| a.resolve().entry.unwrap()).collect();
        let wb: Vec<_> = (0..16).map(|_| b.resolve().entry.unwrap()).collect();
        assert_eq!(wa, wb, "same seed → same walk");
        let distinct: BTreeSet<_> = wa.iter().collect();
        assert!(distinct.len() > 1, "random walk stuck on one entry: {wa:?}");
    }

    #[test]
    fn entry_overlays_the_base_input_fields_and_extras() {
        let mut base = input(Some("sip:base-from@x"), Some("sip:base-to@x"), None);
        base.extras.insert("ring_delay_ms".into(), serde_json::json!(10));
        base.extras.insert("keep".into(), serde_json::json!("base"));
        let mut entry = input(Some("sip:entry-from@x"), None, Some("sip:entry-ruri@x"));
        entry.extras.insert("ring_delay_ms".into(), serde_json::json!(99));
        let pool = BindingPool { mode: BindingMode::Seq, entries: vec![entry] };
        let r = BindingResolver::new(base, Some(pool), 1);
        let got = r.resolve().input;
        assert_eq!(got.core.from.as_deref(), Some("sip:entry-from@x"), "entry wins");
        assert_eq!(got.core.to.as_deref(), Some("sip:base-to@x"), "base fills the gap");
        assert_eq!(got.core.ruri.as_deref(), Some("sip:entry-ruri@x"));
        assert_eq!(got.extras["ring_delay_ms"], serde_json::json!(99), "entry extras win");
        assert_eq!(got.extras["keep"], serde_json::json!("base"), "base extras kept");
    }

    #[test]
    fn extras_strings_expand_too_including_nested_values() {
        let mut base = Input::default();
        base.extras.insert("label".into(), serde_json::json!("call-${seq:3}"));
        base.extras.insert("nested".into(), serde_json::json!({ "inner": ["x-${seq}"] }));
        let r = BindingResolver::new(base, None, 1);
        r.resolve(); // seq 0
        let got = r.resolve().input; // seq 1
        assert_eq!(got.extras["label"], serde_json::json!("call-001"));
        assert_eq!(got.extras["nested"]["inner"][0], serde_json::json!("x-1"));
    }

    #[test]
    fn absent_bindings_is_single_input_behaviour() {
        let r = BindingResolver::new(input(Some("sip:solo@x"), None, None), None, 1);
        let a = r.resolve();
        let b = r.resolve();
        assert_eq!(a.entry, None);
        assert_eq!(a.input.core.from.as_deref(), Some("sip:solo@x"));
        assert_eq!(b.input.core.from.as_deref(), Some("sip:solo@x"));
    }

    #[test]
    fn malformed_tokens_are_linted_and_panic_the_expander() {
        for bad in ["${bogus}", "${seq:}", "${seq:0}", "${rand:x}", "${rand:999}", "pre${seq"] {
            let problems = lint_tokens(&input(Some(bad), None, None));
            assert_eq!(problems.len(), 1, "{bad:?} should lint exactly one problem: {problems:?}");
            let r = BindingResolver::new(input(Some(bad), None, None), None, 1);
            let panicked =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.resolve())).is_err();
            assert!(panicked, "{bad:?} must not expand silently");
        }
        // Literal `$` not opening a token is left alone; `${seq}` beside it works.
        let ok = expand_str("$5 and ${seq}", 3, &mut |_| unreachable!()).unwrap();
        assert_eq!(ok, "$5 and 3");
        assert!(lint_tokens(&input(Some("$5 plain"), None, None)).is_empty());
    }

    #[test]
    fn lint_walks_extras_recursively() {
        let mut i = Input::default();
        i.extras.insert("deep".into(), serde_json::json!({ "a": ["${broken"] }));
        let problems = lint_tokens(&i);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("unclosed"), "{problems:?}");
    }
}
