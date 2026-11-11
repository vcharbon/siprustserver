//! The JSON **Endpoint config** (ADR-0018): the authored document binding
//! logical roles to concrete addresses. Only the authored data model is here;
//! the load generator is its reader.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use scenario_harness::egress::EgressPolicy;

/// The JSON **Endpoint config** (ADR-0018): binds logical roles (alice, bob,
/// charlie, lb) to concrete addresses, plus the agent recv bound and the
/// egress policy, labelled with the Infra shape it addresses.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointConfig {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// The Infra shape label of the topology these addresses reach (e.g.
    /// `loadgen-mux`).
    pub infra_shape: String,
    pub roles: BTreeMap<String, SocketAddr>,
    /// Per-`recv` wait bound handed to every agent (real clock needs a wide one).
    pub recv_timeout_ms: u64,
    /// Optional authored **egress policy** — how a run realizes a logical INVITE
    /// on this topology's wire (`"transparent"` | `"api-call-pin"` |
    /// `{"registrar-aor":{"domain":…}}`); absent = transparent. The load
    /// generator (`loadgen --endpoint-config`) reads it. See
    /// [`EndpointConfig::egress_policy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<EgressPolicySpec>,
}

/// The authored (JSON) form of the egress policy — the serde/schemars-facing
/// spec that converts into the shared domain [`EgressPolicy`]
/// (`scenario_harness::egress`, re-exported as [`crate::egress::EgressPolicy`]).
/// Externally tagged kebab-case, so the values are exactly `"transparent"`,
/// `"api-call-pin"`, and `{"registrar-aor":{"domain":…}}`.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum EgressPolicySpec {
    /// No rewrite: the SUT's own routing reaches the callee.
    Transparent,
    /// Pin the b-leg callee via the proprietary `X-Api-Call` header (the real
    /// cluster / our-b2bua adapter).
    ApiCallPin,
    /// Dial the (primary) callee's registered AOR on `domain` (register front
    /// proxy).
    RegistrarAor { domain: String },
}

impl EgressPolicySpec {
    /// Convert the authored spec into the shared domain policy.
    pub fn to_policy(&self) -> EgressPolicy {
        match self {
            EgressPolicySpec::Transparent => EgressPolicy::Transparent,
            EgressPolicySpec::ApiCallPin => EgressPolicy::ApiCallPin,
            EgressPolicySpec::RegistrarAor { domain } => {
                EgressPolicy::RegistrarAor { domain: domain.clone() }
            }
        }
    }
}

impl EndpointConfig {
    pub fn addr(&self, role: &str) -> SocketAddr {
        *self.roles.get(role).unwrap_or_else(|| panic!("endpoint config is missing role {role:?}"))
    }

    pub fn recv_timeout(&self) -> Duration {
        Duration::from_millis(self.recv_timeout_ms)
    }

    /// The authored egress policy, resolved: [`EgressPolicy::Transparent`] when
    /// the optional `egress` field is absent.
    pub fn egress_policy(&self) -> EgressPolicy {
        self.egress.as_ref().map(EgressPolicySpec::to_policy).unwrap_or(EgressPolicy::Transparent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(egress_json: &str) -> EndpointConfig {
        serde_json::from_str(&format!(
            r#"{{"infraShape":"loadgen-mux","roles":{{"alice":"127.0.0.1:6000"}},"recvTimeoutMs":5000{egress_json}}}"#
        ))
        .unwrap()
    }

    /// The authored `egress` field accepts exactly the three documented forms
    /// and resolves to the shared domain policy; absent = transparent (the
    /// additive-optional guarantee: every pre-existing config parses unchanged).
    #[test]
    fn egress_field_parses_all_three_forms_and_defaults_transparent() {
        assert_eq!(cfg("").egress_policy(), EgressPolicy::Transparent);
        assert_eq!(cfg(r#","egress":"transparent""#).egress_policy(), EgressPolicy::Transparent);
        assert_eq!(cfg(r#","egress":"api-call-pin""#).egress_policy(), EgressPolicy::ApiCallPin);
        assert_eq!(
            cfg(r#","egress":{"registrar-aor":{"domain":"register.example"}}"#).egress_policy(),
            EgressPolicy::RegistrarAor { domain: "register.example".into() }
        );
    }

    /// Round-trip: a config with a policy serializes back to the same authored
    /// form (and an absent policy stays absent — `skip_serializing_if`).
    #[test]
    fn egress_field_round_trips() {
        let with = cfg(r#","egress":"api-call-pin""#);
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains(r#""egress":"api-call-pin""#), "{json}");
        let without = cfg("");
        assert!(!serde_json::to_string(&without).unwrap().contains("egress"));
    }
}
