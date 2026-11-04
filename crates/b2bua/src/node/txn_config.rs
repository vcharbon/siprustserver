//! The worker's transaction-layer tunables.

use std::sync::Arc;

use std::sync::atomic::AtomicBool;

use sip_txn::{BoundPassed, DeferredBound, IdGen, NewCallGate, OnBound, TransactionConfig};

use crate::admission::Refusals;
use crate::config::B2buaConfig;

/// The bound of the worker's transaction event queue: room for a new-INVITE
/// burst beside the in-dialog traffic of established calls, whose keepalive
/// responses a full queue would drop.
pub use crate::config::EVENT_QUEUE_CAPACITY;

/// The worker's inbound UDP queue bound as the transaction layer reads it:
/// it sizes the event queue at four times its value.
const UDP_QUEUE_MAX: usize = EVENT_QUEUE_CAPACITY / 4;

/// The bound of the keepalive lane: the out-of-dialog OPTIONS the proxies'
/// probes and dead notices send, which the router takes ahead of the event
/// queue.
const KEEPALIVE_LANE: usize = 64;

/// The transaction layer's tunables for this worker: its event queue, the
/// deployment's INVITE bounds and CANCEL policy, the worker's shared
/// `refusals`, the ceilings on the deferred backlog
/// ([`crate::admission::deferred_bound`]) unless `ceilings` sets its own,
/// no transaction for an out-of-dialog OPTIONS, which rides the keepalive
/// lane, the worker's advertised SIP address, and the switch on new calls the
/// event-queue gate sets (ADR-0037 item 11), returned with them.
pub(super) fn txn_config(
    config: &B2buaConfig,
    id_gen: &Arc<IdGen>,
    refusals: &Refusals,
    ceilings: Option<DeferredBound>,
    write_path: &crate::repl::WritePath,
    probe: &crate::turn_probe::TurnProbes,
) -> (TransactionConfig, Arc<AtomicBool>) {
    let switch = Arc::new(AtomicBool::new(false));
    let mut txn_config = TransactionConfig {
        udp_queue_max: UDP_QUEUE_MAX,
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
        on_bound: probe.installed().then(|| bound_report(probe, &config.self_ordinal)),
        strict_to_tag: true,
        deferred_bound: None,
        invite_refusals: Some(refusals.memo().clone()),
        // A new normal INVITE is refused on arrival while the event-queue gate
        // refuses, ahead of the queue it would wait in, or once the queue's
        // oldest event has waited what would trip the gate at a single reading:
        // a router held in one turn writes no refusal (ADR-0037 item 11).
        new_call_gate: Some(
            NewCallGate::new(switch.clone(), crate::admission::class_of)
                .refusing_at_age(config.event_queue_gate.refuse_at_age()),
        ),
        // The OPTIONS health answer derives its To-tag from the request
        // (`RouterCtx::options_tagger`).
        stateless_options: true,
        // The router takes a probe ahead of the queued events (ADR-0050 §2).
        keepalive_lane: Some(KEEPALIVE_LANE),
        // The worker's SIP identity names it in a Warning, never a wildcard
        // bind address.
        advertised: Some(hostport(&config.sip_local_ip, config.sip_local_port)),
    };
    debug_assert_eq!(txn_config.event_capacity(), EVENT_QUEUE_CAPACITY);
    txn_config.deferred_bound = Some(
        ceilings.unwrap_or_else(|| crate::admission::deferred_bound(txn_config.event_capacity())),
    );
    (txn_config, switch)
}

/// The report to `probe` of held messages that left at their bound on
/// `node`, by the call their transaction names: the one a request of it
/// carried, else the call `node` derives from the caller's leg, as it does
/// for a new INVITE; `None` when the transaction is gone.
fn bound_report(probe: &crate::turn_probe::TurnProbes, node: &str) -> OnBound {
    let (probe, node) = (probe.clone(), node.to_string());
    Arc::new(move |held: BoundPassed<'_>| {
        let derived = match (held.call_ref, held.call_id, held.from_tag) {
            (None, Some(call_id), Some(from_tag)) => {
                Some(call::derive_call_ref(&node, call_id, from_tag))
            }
            _ => None,
        };
        probe.bound_passed(&node, held.call_ref.or(derived.as_deref()));
    })
}

/// `host:port`, an IPv6 literal host in brackets (RFC 3261 §25.1 `hostport`).
fn hostport(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}
