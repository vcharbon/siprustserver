//! The open **obligation** vocabulary (ADR-0020 X7) — extracted verbatim from
//! the hardcoded limiter/CDR blocks of `invariants::enforce`.
//!
//! An obligation is a per-call consequence that must be discharged **exactly
//! once** at release, **derivable from the persisted [`Call`] snapshot alone**
//! ("the Call is the ledger") and idempotently dischargeable through the
//! existing effect vocabulary. A kind MUST be:
//!
//! - **pure over the snapshot** — `settle`/`owed` read only `&Call` (including
//!   its `ext` slices). Closures and runtime registries are unrepresentable
//!   here by design: the same derivation must produce the same obligations
//!   from a snapshot rehydrated on another node after reclaim.
//! - **total over history** — tolerate snapshots written before the kind
//!   existed (serde defaults / `Option` fields).
//! - **skip-aware** — entries carrying no real allocation are skipped at
//!   derive time (the fail-open limiter precedent).
//! - **idempotent** — `settle` appends only what `effects` does not already
//!   discharge, so a rule that emitted its own cleanup is not doubled, and
//!   settling twice is a no-op. Dedupe semantics are kind-local (the limiter's
//!   `(limiter_id, window)` key vs the CDR's single flag), which is why
//!   derive/dedupe/append live together in one `settle` pass instead of a
//!   framework-owned key round-trip.
//!
//! A kind MUST NOT emit `RemoveCall` or `CancelAllTimers` — the enforcer owns
//! those, and `RemoveCall`-last is the release ordering invariant.
//!
//! [`ObligationSet::settle`] is called by `invariants::enforce` on the
//! `→ Terminated` transition — the single discharge point every terminal path
//! (rule-driven, timer-driven, and the future reaper's strike-2 discharge)
//! funnels through.

use call::Call;
use std::collections::HashSet;

use crate::effects::{BufferedObservabilityEffect, HandlerEffects, SoftBoundedEffect};

/// One owed release, as data — the pure audit view ([`ObligationSet::owed`])
/// used for logging and tests; the discharging side effect is expressed through
/// the existing [`HandlerEffects`] vocabulary, never through this struct.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Obligation {
    /// Stable kind id ("limiter", "cdr", …) — metrics labels / audit lines only.
    pub kind: &'static str,
    /// Dedupe key, unique within `kind` for one call
    /// (limiter: `"{limiter_id}:{origin_window}"`; cdr: `"cdr"`).
    pub key: String,
}

/// One kind of per-call resource owed at release. See the module doc for the
/// contract (pure over the snapshot, total over history, skip-aware,
/// idempotent; never `RemoveCall`/`CancelAllTimers`).
pub trait ObligationKind: Send + Sync + 'static {
    /// Stable identity for metrics/audit.
    fn id(&self) -> &'static str;

    /// Append the discharging effect(s) for every obligation `call` still owes
    /// that `effects` does not already discharge. Idempotent.
    fn settle(&self, call: &Call, effects: &mut HandlerEffects);

    /// Pure audit view: what this call owes, ignoring any already-emitted
    /// effects.
    fn owed(&self, call: &Call) -> Vec<Obligation>;
}

/// The closed-at-startup registry of kinds. Built once in `b2bua_core` and
/// shared via `RouterCtx` by every `enforce` call site — one derivation, every
/// terminal path. Registration order is discharge order within each effect
/// lane.
pub struct ObligationSet {
    kinds: Vec<Box<dyn ObligationKind>>,
}

impl ObligationSet {
    /// The two core kinds, in this order: [`LimiterObligations`],
    /// [`CdrObligation`].
    pub fn core() -> Self {
        Self {
            kinds: vec![Box::new(LimiterObligations), Box::new(CdrObligation)],
        }
    }

    /// Extension point: a service / the runner registers an additional kind at
    /// startup (wiring time only — the set is never mutated after spawn).
    pub fn with(mut self, kind: Box<dyn ObligationKind>) -> Self {
        self.kinds.push(kind);
        self
    }

    /// Derive-dedupe-append for every kind, in registration order. Idempotent
    /// w.r.t. effects already present.
    pub fn settle(&self, call: &Call, effects: &mut HandlerEffects) {
        for kind in &self.kinds {
            kind.settle(call, effects);
        }
    }

    /// Pure audit view across all kinds.
    pub fn owed(&self, call: &Call) -> Vec<Obligation> {
        self.kinds.iter().flat_map(|k| k.owed(call)).collect()
    }
}

/// Kind `"limiter"` — every recorded hold is decremented exactly once on
/// termination (the strong INCR↔DECR invariant). Fail-open admissions
/// (`increment_succeeded == Some(false)`) carry no real increment, so they are
/// skipped. Dedupes against any release a rule already emitted. Verbatim
/// extraction of the former `invariants::enforce` limiter block.
pub struct LimiterObligations;

impl ObligationKind for LimiterObligations {
    fn id(&self) -> &'static str {
        "limiter"
    }

    fn settle(&self, call: &Call, effects: &mut HandlerEffects) {
        let already: HashSet<(String, i64)> = effects
            .soft
            .iter()
            .map(|SoftBoundedEffect::DecrementLimiter { limiter_id, window }| {
                (limiter_id.clone(), *window)
            })
            .collect();
        for entry in &call.limiter_entries {
            if entry.increment_succeeded == Some(false) {
                continue;
            }
            let key = (entry.limiter_id.clone(), entry.origin_window);
            if already.contains(&key) {
                continue;
            }
            effects.soft.push(SoftBoundedEffect::DecrementLimiter {
                limiter_id: entry.limiter_id.clone(),
                window: entry.origin_window,
            });
        }
    }

    fn owed(&self, call: &Call) -> Vec<Obligation> {
        call.limiter_entries
            .iter()
            .filter(|e| e.increment_succeeded != Some(false))
            .map(|e| Obligation {
                kind: "limiter",
                key: format!("{}:{}", e.limiter_id, e.origin_window),
            })
            .collect()
    }
}

/// Kind `"cdr"` — exactly one CDR per terminated call. The single-element
/// derivation IS the exactly-one-CDR promise; the dedupe is "a `WriteCdr` is
/// already queued". Verbatim extraction of the former `invariants::enforce`
/// CDR block.
pub struct CdrObligation;

impl ObligationKind for CdrObligation {
    fn id(&self) -> &'static str {
        "cdr"
    }

    fn settle(&self, _call: &Call, effects: &mut HandlerEffects) {
        if !effects
            .buffered
            .iter()
            .any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr))
        {
            effects.buffered.push(BufferedObservabilityEffect::WriteCdr);
        }
    }

    fn owed(&self, _call: &Call) -> Vec<Obligation> {
        vec![Obligation { kind: "cdr", key: "cdr".into() }]
    }
}
