//! The worker's transaction-layer tunables.

use std::sync::Arc;

use sip_txn::{DeferredBound, IdGen, TransactionConfig};

use crate::admission::Refusals;
use crate::config::B2buaConfig;

/// The transaction layer's tunables for this worker: its event queue, the
/// deployment's INVITE bounds and CANCEL policy, the worker's shared
/// `refusals`, the ceilings on the deferred backlog
/// ([`crate::admission::deferred_bound`]) unless `ceilings` sets its own,
/// no transaction for an out-of-dialog OPTIONS, and the worker's advertised
/// SIP address.
pub(super) fn txn_config(
    config: &B2buaConfig,
    id_gen: &Arc<IdGen>,
    refusals: &Refusals,
    ceilings: Option<DeferredBound>,
    write_path: &crate::repl::WritePath,
    probe: &crate::turn_probe::TurnProbes,
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
        // An INVITE's 100 is the router's to release once the call's write is
        // applied at its backup (ADR-0055 §3); the layer sends it at the
        // §17.2.1 bound otherwise.
        trying_bound_ms: sip_txn::timers::TRYING_BOUND,
        // The ACK of a callee's non-2xx final is the router's to release with
        // the turn that took the final (ADR-0055 §3); the layer sends it at
        // the write path's ACK bound otherwise.
        ack_bound_ms: u64::try_from(write_path.ack_bound.as_millis()).unwrap_or(u64::MAX),
        // The answers to a CANCEL likewise, with the turn that took it.
        cancel_bound_ms: u64::try_from(write_path.cancel_bound.as_millis()).unwrap_or(u64::MAX),
        // A probe hears of what left at its bound, before its turn's write.
        on_bound: probe.installed().then(|| {
            let (probe, node) = (probe.clone(), config.self_ordinal.clone());
            Arc::new(move || probe.bound_passed(&node)) as Arc<dyn Fn() + Send + Sync>
        }),
        strict_to_tag: true,
        deferred_bound: None,
        invite_refusals: Some(refusals.memo().clone()),
        // The OPTIONS health answer derives its To-tag from the request
        // (`RouterCtx::options_tagger`).
        stateless_options: true,
        // The worker's SIP identity names it in a Warning, never a wildcard
        // bind address.
        advertised: Some(hostport(&config.sip_local_ip, config.sip_local_port)),
    };
    txn_config.deferred_bound = Some(
        ceilings.unwrap_or_else(|| crate::admission::deferred_bound(txn_config.event_capacity())),
    );
    txn_config
}

/// `host:port`, an IPv6 literal host in brackets (RFC 3261 §25.1 `hostport`).
fn hostport(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}
