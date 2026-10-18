//! The worker's transaction-layer tunables.

use std::sync::Arc;

use sip_txn::{DeferredBound, IdGen, TransactionConfig};

use crate::admission::Refusals;
use crate::config::B2buaConfig;

/// The transaction layer's tunables for this worker: its event queue, the
/// deployment's INVITE bounds and CANCEL policy, the worker's shared
/// `refusals`, the ceilings on the deferred backlog
/// ([`crate::admission::deferred_bound`]) unless `ceilings` sets its own, and
/// no transaction for an out-of-dialog OPTIONS.
pub(super) fn txn_config(
    config: &B2buaConfig,
    id_gen: &Arc<IdGen>,
    refusals: &Refusals,
    ceilings: Option<DeferredBound>,
) -> TransactionConfig {
    let mut txn_config = TransactionConfig {
        // Sizes the events channel at 4096: room for a new-INVITE burst
        // beside the in-dialog traffic of established calls, whose keepalive
        // responses a full channel would drop.
        udp_queue_max: 1024,
        id_gen: id_gen.clone(),
        // The deployment's initial-INVITE bound (default 158 s):
        // `config.validate()` keeps `setup_timeout_ms` strictly below
        // it, so the rules path always gives up before the txn layer.
        invite_initial_timeout_ms: config.invite_txn_timeout_ms(),
        // The TU's first-response deadline on an initial INVITE
        // (ADR-0044; default Timer B, which arms nothing).
        invite_first_response_timeout_ms: config.invite_first_response_timeout_ms(),
        strict_to_tag: true,
        deferred_bound: None,
        invite_refusals: Some(refusals.memo().clone()),
        // The OPTIONS health answer derives its To-tag from the request
        // (`RouterCtx::options_tagger`).
        stateless_options: true,
    };
    txn_config.deferred_bound = Some(
        ceilings.unwrap_or_else(|| crate::admission::deferred_bound(txn_config.event_capacity())),
    );
    txn_config
}
