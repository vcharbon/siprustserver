//! The lane's number allocation (`PCAP2TEST_PIVOT_V3.md` §4.3): every tier-2
//! POSITION the document names bound to a number, plus every identity bound in
//! each dial form it declares.
//!
//! A captured document's `identities[].observed` is the anonymized value the
//! capture carried; the lane allocates it back by default, because the system
//! routes by its own decision and the number only has to be consistent and
//! dialable. A driver that needs a ROUTABLE number (a number a backend double
//! routes on, a number provisioned on a real backend) states it in the
//! run-spec's `run.identities`, which wins over the allocation here.

use std::collections::BTreeMap;

use pivot_interpreter::Plan;
use pivot_schema::bundle::IdentityBindings;

/// The default number for one identity: the observed userpart, else the name
/// squashed into a digit-safe token.
fn default_number(observed: Option<&str>, name: &str) -> String {
    observed
        .map(|o| o.split('@').next().unwrap_or(o).to_string())
        .unwrap_or_else(|| name.replace(['-', '.'], ""))
}

/// Allocate positions and bindings for `plan`, with `seed` (the run-spec's
/// explicit bindings) winning per (name, form). Every identity the registry
/// declares is bound in every form it declares, dialled or not: a `${num:…}`
/// may name a party no attempt dials (a Refer-To's transfer target), and an
/// unbound identity refuses at emission. The positional map reads the merged
/// binding, so an explicit run-spec number and the positional emission agree.
pub fn lane_numbers(
    plan: &Plan,
    seed: IdentityBindings,
) -> (BTreeMap<String, String>, IdentityBindings) {
    let document = plan.document();
    let mut bindings = seed;
    for identity in &document.identities {
        for form in &identity.forms {
            if bindings.resolve(&identity.name, form).is_err() {
                let number = default_number(identity.observed.as_deref(), &identity.name);
                bindings = bindings.bind(&identity.name, form, number);
            }
        }
    }
    let resolved = |name: &str| -> Option<String> {
        let identity = document.identities.iter().find(|i| i.name == name)?;
        identity
            .forms
            .iter()
            .find_map(|form| bindings.resolve(name, form).ok())
            .map(str::to_string)
            .or_else(|| Some(default_number(identity.observed.as_deref(), name)))
    };
    let qualified = document.calls.len() > 1;
    let mut numbers = BTreeMap::new();
    for call in &document.calls {
        let prefix = if qualified { format!("{}.", call.id) } else { String::new() };
        let caller_identity = document
            .legs
            .iter()
            .find(|l| l.id == call.caller_leg)
            .and_then(|l| document.actors.iter().find(|a| a.id == l.actor))
            .and_then(|a| a.identity.clone());
        if let Some(number) = caller_identity.as_deref().and_then(resolved) {
            numbers.insert(format!("{prefix}caller"), number);
        }
        for attempt in &call.attempts {
            if let Some(number) = resolved(&attempt.callee.identity) {
                numbers.insert(
                    format!("{prefix}called[{}][{}]", attempt.branch, attempt.position),
                    number,
                );
            }
        }
    }
    (numbers, bindings)
}
