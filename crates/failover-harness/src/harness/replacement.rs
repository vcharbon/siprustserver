//! A replacement incarnation of an ordinal stood up beside the running one:
//! the orchestrator recreating a worker whose old process is still up.

use std::net::SocketAddr;

use repl_net::transport::ReplicationNetwork;

use super::{FailoverHarness, ReplicatedB2buaSut};
use crate::views::Belief;

impl FailoverHarness {
    /// **Start a replacement incarnation of `ordinal` ALONGSIDE the running
    /// one** — the orchestrator recreating a withdrawn worker while its old
    /// process is still up. The new incarnation comes up at gen+1 on a fresh SIP
    /// address (a new pod IP, like a reboot) AND a fresh replication listen
    /// address, since the old incarnation still holds the declared one; the
    /// cluster's resolver map is re-pointed at it, so peers reach the
    /// replacement on their next connect (ADR-0012 D3). The old incarnation is
    /// untouched — the caller keeps its handle and decides when it dies.
    ///
    /// The replacement's SIP + repl binds are asserted, never assumed: a core
    /// whose replication listen fails is silently unreachable, which would read
    /// as a replication bug three assertions later.
    pub async fn spawn_replacement(&mut self, ordinal: &str) -> ReplicatedB2buaSut {
        let spec = self
            .worker_specs
            .get(ordinal)
            .cloned()
            .unwrap_or_else(|| panic!("worker {ordinal} was never spawned"));
        let gen = self.views.max_gen(ordinal) + 1;
        let octet = u8::try_from(gen).expect("gen fits a host octet (< 256 incarnations)");
        let sip_addr = SocketAddr::from(([127, 0, octet, 1], spec.sip_base_addr.port()));
        let declared = self.repl_addrs[ordinal];
        let listen_addr = SocketAddr::from((
            declared.ip(),
            declared.port() + 100 * u16::try_from(gen - 1).expect("gen fits"),
        ));
        // Peers must find the replacement, not the incarnation it replaces, and
        // the fabric must read the ordinal's streams as the replacement's, so a
        // fault named on the ordinal reaches them (both maps before the spawn:
        // the streams it opens are attributed to this address).
        self.repl_resolver.lock().unwrap().insert(ordinal.to_string(), listen_addr);
        self.repl_sim.declare_endpoint(ordinal, listen_addr);

        let sut = self.spawn_incarnation(ordinal, &spec, gen, sip_addr, listen_addr).await;
        self.assert_repl_listening(ordinal, listen_addr).await;
        self.mark(
            ordinal,
            None,
            "replacement",
            &format!(
                "gen={gen} alongside the running incarnation; sip={sip_addr} repl={listen_addr}"
            ),
        );
        self.views.record("orchestrator", ordinal, Belief::Running { gen }, "spawn_replacement");
        sut
    }

    /// Assert `ordinal`'s new incarnation really owns `listen_addr` on the repl
    /// fabric. `B2buaCore` spawns its replication server in a task and swallows
    /// the listen error, so a duplicate bind would otherwise show up only as a
    /// peer that never pulls.
    async fn assert_repl_listening(&self, ordinal: &str, listen_addr: SocketAddr) {
        sip_clock::testkit::settle().await;
        match self.repl_sim.connect(listen_addr).await {
            Ok(_probe) => {}
            Err(e) => panic!(
                "{ordinal}'s replacement did not take its replication listen address                  {listen_addr} ({e:?}) — its peers would never reach it",
            ),
        }
    }
}
