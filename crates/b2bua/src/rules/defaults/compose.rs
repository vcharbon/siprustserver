//! Composition of the default rule set: which rule families participate and
//! in which registration (priority) order, plus the compose-time opt-out seam.
//! The rules themselves live in [`super::core_rules`] and the sibling
//! `rules::*` family modules.

use crate::rules::model::RuleDefinition;

use super::core_rules::core_rules;

/// Compose-time selection of which built-in CORE machines participate in the
/// default rule set (ADR-0016 opt-out seam). Default = every built-in
/// included, so [`default_rules`] is behaviour-preserving. A downstream
/// integrator that ships its OWN subscription-gated transfer machine
/// (a SERVICE_LAYER service) uses [`without_core_refer_transfer`](Self::without_core_refer_transfer)
/// so it fully owns REFER; the opt-out is reachable via the spawn seam
/// ([`B2buaDeps::compose`](crate::B2buaDeps)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposeOptions {
    /// Include the upstream `refer_transfer` seed (`transfer-intercept-refer` /
    /// `transfer-reject-a-leg-refer` / `transfer-reject-replaces`) **and** its
    /// machine-gated SERVICE_LAYER rules (default `true`). Set `false` and a
    /// REFER is never intercepted, whatever the route activated — it falls
    /// through to the transparent `relay-refer` path, forwarded to the peer leg
    /// like INFO. A downstream transfer machine then owns the *subscribed*
    /// REFER; the *unsubscribed* one relays transparently (RFC 3515 implicit
    /// subscription rides the dialog, so its NOTIFYs relay through too).
    ///
    /// Included (the default), the seed still only intercepts a call whose
    /// route activated `features.refer` — inclusion is the compose-time
    /// permission, the feature arm is the per-call directive.
    pub core_refer_transfer: bool,
}

impl Default for ComposeOptions {
    fn default() -> Self {
        Self { core_refer_transfer: true }
    }
}

impl ComposeOptions {
    /// Exclude the upstream `refer_transfer` seed + machine-gated rules. The
    /// composed rule set then relays every in-dialog REFER transparently via
    /// `relay-refer`, even on a call whose route activated `features.refer`.
    pub fn without_core_refer_transfer(mut self) -> Self {
        self.core_refer_transfer = false;
        self
    }
}

/// The ordered basic-B2BUA rule list. The SERVICE_LAYER `relayFirst18xTo180`
/// rules are appended at the end; they are dormant unless a call activates the
/// feature (their column+filter gate keeps them out of `pick_ranked` otherwise),
/// and `pick_ranked` ranks SERVICE_LAYER above CORE so they win when active.
pub fn default_rules() -> Vec<RuleDefinition> {
    default_rules_with(&ComposeOptions::default())
}

/// [`default_rules`] under an explicit [`ComposeOptions`] — the compose-time
/// opt-out seam. Threaded from [`B2buaDeps::compose`](crate::B2buaDeps) through
/// `spawn_with_overload`, so a downstream runner selects it without touching the
/// rule tables directly.
pub fn default_rules_with(options: &ComposeOptions) -> Vec<RuleDefinition> {
    // The REFER seed rules are CORE_LAYER and must out-rank the generic
    // `relay-refer`/`relay-non-invite` REFER relay; registration order (earlier
    // wins within a layer) puts them first. Their match columns + their
    // `features.refer` / `no_transfer_active` filters keep them inert for
    // non-REFER traffic and for a call whose route never activated local REFER
    // processing. Excluded when a downstream owns REFER via its own transfer
    // machine.
    let mut rules = Vec::new();
    if options.core_refer_transfer {
        rules.extend(crate::rules::refer_transfer::transfer_seed_rules());
    }
    // Release-event / established-call-reroute rules: CORE, registered BEFORE
    // the generic core rules so the reroute-gated matches (filtered on the
    // `reroute` slice — inert otherwise) out-rank
    // `confirm-dialog`/`relay-provisional`/`route-failure` by order.
    rules.extend(crate::rules::release_reroute::release_reroute_rules());
    rules.extend(core_rules());
    rules.extend(crate::rules::relay_first_18x::relay_first_18x_rules());
    rules.extend(crate::rules::promote_pem::promote_pem_rules());
    if options.core_refer_transfer {
        // The machine-gated transfer rules stay dormant without the seed (the
        // slice is never installed), but a downstream owning REFER wants the
        // whole upstream machine gone — exclude them together with the seed.
        rules.extend(crate::rules::refer_transfer::transfer_rules());
    }
    rules
}
