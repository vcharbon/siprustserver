use super::*;
use crate::config::B2buaConfig;
use sip_txn::IdGen;

/// The worker always bounds the deferred backlog, at one and two event
/// queues.
#[test]
fn the_worker_bounds_the_deferred_backlog() {
    let refusals = Refusals::new(5, 0, 16, &IdGen::seeded(2));
    let config = txn_config(&B2buaConfig::default(), &Arc::new(IdGen::seeded(1)), &refusals, None);
    assert_eq!(config.event_capacity(), 4096);
    let bound = config.deferred_bound.expect("the worker sets a deferred bound");
    assert_eq!((bound.normal, bound.emergency), (4096, 8192));
}

/// The worker holds no transaction for an out-of-dialog OPTIONS.
#[test]
fn the_worker_answers_out_of_dialog_options_statelessly() {
    let refusals = Refusals::new(5, 0, 16, &IdGen::seeded(2));
    let config = txn_config(&B2buaConfig::default(), &Arc::new(IdGen::seeded(1)), &refusals, None);
    assert!(config.stateless_options);
}
