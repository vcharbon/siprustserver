//! The seam a consumer's sweep shape states its call through (ADR-0055 §11):
//! callee roles that share bob's address, demultiplexed by the Request-URI
//! user part, and a shape left out of the split that never heals.

use failover_harness::cluster::{Cluster, ClusterRecipe};
use failover_harness::sweep::{sweep, Incident, Parties, Ratchet, SweepShape, BOB};
use scenario_harness::actor::ActorScenario;

/// A shape left out of the split that never heals, whose call no cell may
/// place.
struct Unsplit;

impl SweepShape for Unsplit {
    fn id(&self) -> &'static str {
        "unsplit"
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        panic!("a shape left out of the split places no call")
    }

    fn splits_unhealed(&self) -> bool {
        false
    }
}

#[tokio::test(start_paused = true)]
async fn callee_roles_sharing_bob_s_address_are_bound_in_the_call_s_environment() {
    let bob = BOB.parse().expect("bob's address");
    let cluster = Cluster::start("shared_callees", &ClusterRecipe::routing_to(bob)).await;
    cluster.fh.disarm_report_on_drop();
    let shared = [("first", "100"), ("second", "200"), ("second", "201")];
    let parties = Parties::bind(&cluster.fh, &shared).await;
    assert_eq!(parties.bob.name(), "first", "bob is the first shared role");
    let env = parties.env(cluster.proxy.addr(), "shared_callees");
    for role in ["first", "second"] {
        assert_eq!(env.callee_agent(role).addr(), bob, "{role} answers on bob's address");
    }
}

#[test]
fn a_shape_left_out_of_the_unhealed_split_runs_no_cell_of_it() {
    let ratchet = Ratchet::load(concat!(env!("CARGO_MANIFEST_DIR"), "/no-such.ratchet"));
    sweep(&Unsplit, Incident::Split, &ratchet);
}
