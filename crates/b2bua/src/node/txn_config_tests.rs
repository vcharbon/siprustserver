use super::*;
use crate::config::B2buaConfig;
use sip_txn::IdGen;

/// The worker always bounds the deferred backlog, at one and two event
/// queues.
#[test]
fn the_worker_bounds_the_deferred_backlog() {
    let refusals = Refusals::new(5, 0, 16, &IdGen::seeded(2));
    let config = txn_config(
        &B2buaConfig::default(),
        &Arc::new(IdGen::seeded(1)),
        &refusals,
        None,
        &crate::repl::WritePath::default(),
        &Default::default(),
    );
    assert_eq!(config.event_capacity(), 4096);
    let bound = config.deferred_bound.expect("the worker sets a deferred bound");
    assert_eq!((bound.normal, bound.emergency), (4096, 8192));
}

/// The worker holds no transaction for an out-of-dialog OPTIONS.
#[test]
fn the_worker_answers_out_of_dialog_options_statelessly() {
    let refusals = Refusals::new(5, 0, 16, &IdGen::seeded(2));
    let config = txn_config(
        &B2buaConfig::default(),
        &Arc::new(IdGen::seeded(1)),
        &refusals,
        None,
        &crate::repl::WritePath::default(),
        &Default::default(),
    );
    assert!(config.stateless_options);
}

/// The worker names itself by its advertised address in a Warning, never by
/// a wildcard bind.
#[test]
fn the_worker_is_advertised_by_its_sip_identity() {
    let refusals = Refusals::new(5, 0, 16, &IdGen::seeded(2));
    let b2bua = B2buaConfig {
        sip_local_ip: "203.0.113.5".to_string(),
        sip_local_port: 5070,
        ..B2buaConfig::default()
    };
    let config = txn_config(
        &b2bua,
        &Arc::new(IdGen::seeded(1)),
        &refusals,
        None,
        &crate::repl::WritePath::default(),
        &Default::default(),
    );
    assert_eq!(config.advertised.as_deref(), Some("203.0.113.5:5070"));

    let v6 = B2buaConfig { sip_local_ip: "2001:db8::5".to_string(), ..b2bua };
    let config = txn_config(
        &v6,
        &Arc::new(IdGen::seeded(1)),
        &refusals,
        None,
        &crate::repl::WritePath::default(),
        &Default::default(),
    );
    assert_eq!(config.advertised.as_deref(), Some("[2001:db8::5]:5070"));
}

/// The layer holds a callee's non-2xx ACK and a CANCEL's answers for the
/// write path's bounds.
#[test]
fn the_ack_and_cancel_bounds_are_the_write_paths() {
    let refusals = Refusals::new(5, 0, 16, &IdGen::seeded(2));
    let path = crate::repl::WritePath {
        ack_bound: std::time::Duration::from_millis(300),
        cancel_bound: std::time::Duration::from_millis(250),
        ..Default::default()
    };
    let config = txn_config(
        &B2buaConfig::default(),
        &Arc::new(IdGen::seeded(1)),
        &refusals,
        None,
        &path,
        &Default::default(),
    );
    assert_eq!((config.ack_bound_ms, config.cancel_bound_ms), (300, 250));
}
