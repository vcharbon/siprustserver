//! The parties of a sweep cell: the caller, the callee, a second callee, the
//! transfer target and a media server, each bound on its own address, the
//! callee roles a shape shares on bob's address, and how they answer what the
//! cluster still sends once their call is over.

use std::net::SocketAddr;

use scenario_harness::realcall::CallEnv;
use scenario_harness::{Agent, Inbound};

use crate::FailoverHarness;

/// The caller.
pub const ALICE: &str = "127.0.0.1:5060";
/// The callee every shape's decision routes to by default.
pub const BOB: &str = "127.0.0.1:5070";
/// The second callee (a reroute's target).
pub const BOB2: &str = "127.0.0.1:5071";
/// A media server a service dials.
pub const MEDIA: &str = "127.0.0.1:5072";
/// The transfer target.
pub const CHARLIE: &str = "127.0.0.1:5090";

/// The callee role a media server answers as in a [`CallEnv`].
pub const MEDIA_ROLE: &str = "media";

/// Every party of a cell.
pub struct Parties {
    pub alice: Agent,
    pub bob: Agent,
    pub bob2: Agent,
    pub media: Agent,
    pub charlie: Agent,
    /// The callee roles sharing bob's address, each with its agent, in
    /// declaration order.
    pub shared: Vec<(&'static str, Agent)>,
}

impl Parties {
    /// Bind every party on `fh`'s signalling fabric. `shared` lists the
    /// callee roles that share bob's address as `(role, prefix)` pairs
    /// ([`SweepShape::shared_callees`](super::SweepShape::shared_callees)):
    /// empty, bob is bound alone; otherwise bob is the first role.
    pub async fn bind(fh: &FailoverHarness, shared: &[(&'static str, &'static str)]) -> Parties {
        let shared = if shared.is_empty() {
            Vec::new()
        } else {
            let group = fh.callee_group(BOB, shared).await;
            let mut roles: Vec<&'static str> = Vec::new();
            for (role, _) in shared {
                if !roles.contains(role) {
                    roles.push(role);
                }
            }
            roles.into_iter().map(|role| (role, group.agent(role))).collect()
        };
        let bob = match shared.first() {
            Some((_, agent)) => agent.clone(),
            None => fh.agent("bob", BOB).await,
        };
        Parties {
            alice: fh.agent("alice", ALICE).await,
            bob,
            bob2: fh.agent("bob2", BOB2).await,
            media: fh.agent(MEDIA_ROLE, MEDIA).await,
            charlie: fh.agent("charlie", CHARLIE).await.transfer_target(),
            shared,
        }
    }

    /// The call environment of a shape run through `via`, every callee role
    /// bound.
    pub fn env(&self, via: SocketAddr, token: &str) -> CallEnv<'_> {
        let env = CallEnv::for_functional(
            &self.alice,
            &self.bob,
            Some(&self.charlie),
            via,
            "X-Sweep-Id",
            token,
        )
        .with_bob2(&self.bob2)
        .with_callee(MEDIA_ROLE, &self.media);
        self.shared.iter().fold(env, |env, (role, agent)| env.with_callee(role, agent))
    }

    fn all(&self) -> impl Iterator<Item = &Agent> {
        [&self.alice, &self.bob, &self.bob2, &self.media, &self.charlie]
            .into_iter()
            .chain(self.shared.iter().map(|(_, agent)| agent))
    }

    /// Answer every request queued at a party whose call is over, as a UA
    /// holding no dialog does: an ACK takes no answer, a request in a dialog
    /// draws `481` (RFC 3261 §12.2.2, §15.1.2), any other request `200`.
    pub async fn answer_strays(&self) {
        for agent in self.all() {
            while let Some(inbound) = agent.take_queued_inbound().await {
                let Inbound::Request(mut txn) = inbound else {
                    continue;
                };
                let request = txn.request();
                if request.method().as_str() == "ACK" {
                    continue;
                }
                if request.to().tag().is_some() || request.method().as_str() == "CANCEL" {
                    txn.respond(481, "Call/Transaction Does Not Exist").await;
                } else {
                    txn.respond(200, "OK").await;
                }
            }
        }
    }
}
