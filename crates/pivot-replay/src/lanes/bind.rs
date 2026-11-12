//! Binding a document's endpoints on real sockets, and reporting where they
//! landed.
//!
//! Where they landed is not always where they were asked for: an endpoint stated
//! at port `0` takes whichever port the OS has free, which is what lets N cells
//! of one batch bind at once instead of queueing on one port plan. The lane needs
//! the bound address back — the SUT is told to egress there — so the two views
//! travel together and nothing downstream reads the `0`.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use scenario_harness::{Agent, AgentBinder};

use crate::{Case, Failure, NoDial};

/// A document's endpoints, bound.
pub struct Bound {
    /// Endpoint id → the agent playing it.
    pub agents: BTreeMap<String, Agent>,
    /// Endpoint id → the socket it ACTUALLY listens on.
    pub addrs: BTreeMap<String, SocketAddr>,
}

impl Bound {
    /// The socket the lane's egress directive targets, by endpoint id — the
    /// bound one, so a `0` in the run-spec never reaches the SUT's config.
    ///
    /// A document that dials nobody names no callee endpoint; where its egress
    /// goes instead is [`NoDial`]'s to say, for the reasons
    /// [`Case::egress_addr`](crate::Case) gives.
    pub fn egress_addr(
        &self,
        endpoint: &str,
        no_dial: Option<NoDial>,
    ) -> Result<SocketAddr, Failure> {
        crate::egress_target(&self.addrs, endpoint, no_dial)
    }
}

/// Bind every endpoint the document declares, at the address the lane resolved
/// for it.
pub async fn endpoints(binder: &AgentBinder, case: &Case) -> Bound {
    let mut agents = BTreeMap::new();
    let mut addrs = BTreeMap::new();
    for endpoint in &case.document.endpoints {
        let asked = case.endpoint_addrs[&endpoint.id];
        let agent = binder.agent(endpoint.id.as_str(), &asked.to_string()).await;
        addrs.insert(endpoint.id.clone(), agent.addr());
        agents.insert(endpoint.id.clone(), agent);
    }
    Bound { agents, addrs }
}
