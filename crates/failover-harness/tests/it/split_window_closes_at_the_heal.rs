//! A split accepts that its two copies mint one in-dialog CSeq (ADR-0055 §8,
//! §11); the heal leaves one copy numbering the call's requests, so the
//! window closes there.

use failover_harness::cluster::{Cluster, ClusterRecipe};
use failover_harness::sweep::{Parties, BOB, PRIMARY};
use failover_harness::RULE_CSEQ_IN_DIALOG_ORDER;

#[tokio::test(start_paused = true)]
async fn the_duplicate_cseq_window_closes_at_the_heal() {
    let bob = BOB.parse().expect("bob's address");
    let mut cluster = Cluster::start("split_window", &ClusterRecipe::routing_to(bob)).await;
    cluster.fh.disarm_report_on_drop();
    let parties = Parties::bind(&cluster.fh, &[]).await;
    let placement = cluster.place_next_call(&parties.alice, PRIMARY);

    cluster.split(&placement);
    assert!(cluster.fh.rfc_window_open(RULE_CSEQ_IN_DIALOG_ORDER), "open while split");
    cluster.heal(&placement);
    assert!(!cluster.fh.rfc_window_open(RULE_CSEQ_IN_DIALOG_ORDER), "closed at the heal");
}
