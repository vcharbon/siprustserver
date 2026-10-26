//! A call whose backup refused its replica has no backup for life (ADR-0055
//! §3): once the refusal is in, its turns hand nothing to replication, so no
//! turn of it offers a write the backup could apply.

use failover_harness::cluster::ClusterRecipe;
use failover_harness::sweep::catalogue::Basic;
use failover_harness::sweep::{baseline, SweepShape, BOB};
use scenario_harness::actor::ActorScenario;

/// The basic call, its backup refusing every replica.
struct Unbacked;

impl SweepShape for Unbacked {
    fn id(&self) -> &'static str {
        "unbacked"
    }

    fn recipe(&self) -> ClusterRecipe {
        let mut recipe = ClusterRecipe::routing_to(BOB.parse().expect("bob's address"));
        recipe.replica.backup.calls = Some(0);
        recipe
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Basic.scenario()
    }
}

/// The refusal answers the call's first write; every turn from the third
/// on is past it.
#[test]
fn a_call_with_no_backup_writes_nothing_once_refused() {
    let turns = baseline(&Unbacked);
    let wrote: Vec<bool> = turns.iter().map(|t| t.wrote).collect();
    assert!(wrote.len() > 2, "{wrote:?}");
    assert!(wrote[2..].iter().all(|w| !w), "no write after the refusal: {wrote:?}");
}
