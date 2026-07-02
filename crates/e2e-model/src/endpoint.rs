//! The JSON **Endpoint config** (ADR-0018): the authored document binding one
//! Infra shape's logical roles to concrete addresses. The Infra shapes
//! themselves (the trait + the concrete topologies, which spawn the SUT) live
//! in `e2e-core`; only the authored data model is here.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

/// The JSON **Endpoint config** (ADR-0018): binds one Infra shape's logical
/// roles (alice, bob1, lb, b2bua) to concrete addresses, plus the agent recv
/// bound. Addresses are infra-specific, so a config names the Infra shape it
/// is for — `build` rejects a mismatch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointConfig {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// The Infra shape these addresses bind (e.g. `fake-lsbc-b2bua`).
    pub infra_shape: String,
    pub roles: BTreeMap<String, SocketAddr>,
    /// Per-`recv` wait bound handed to every agent (real clock needs a wide one).
    pub recv_timeout_ms: u64,
    /// One-hop simulated transit delay (fake only; coerced to ≥1ms).
    #[serde(default)]
    pub transit_delay_ms: u64,
}

impl EndpointConfig {
    pub fn addr(&self, role: &str) -> SocketAddr {
        *self
            .roles
            .get(role)
            .unwrap_or_else(|| panic!("endpoint config is missing role {role:?}"))
    }

    pub fn recv_timeout(&self) -> Duration {
        Duration::from_millis(self.recv_timeout_ms)
    }

    /// The RTP address for a media-exchanging agent: an explicit
    /// `"<role>.rtp"` role wins; otherwise the agent's signaling IP with
    /// `default_port` (fine on the simulated fabric; real configs should pin
    /// `<role>.rtp` to avoid port clashes).
    pub fn media_addr(&self, role: &str, default_port: u16) -> SocketAddr {
        self.roles
            .get(&format!("{role}.rtp"))
            .copied()
            .unwrap_or_else(|| SocketAddr::new(self.addr(role).ip(), default_port))
    }

    /// Fail loudly when a config authored for one infra is handed to another.
    pub fn assert_binds(&self, infra_id: &str) {
        assert_eq!(
            self.infra_shape, infra_id,
            "endpoint config is for infra {:?}, not {infra_id:?}",
            self.infra_shape
        );
    }
}
