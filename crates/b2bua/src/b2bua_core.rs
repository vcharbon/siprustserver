//! `B2buaCore` — composes the dispatcher + router + call store + transaction
//! layer + timer service + decision engine + CDR writer over a bound UDP
//! endpoint, and spawns the router loop. Port of `B2buaCore.ts`'s layer
//! composition. Construct it over an endpoint (in tests, `Harness::bind_sut`),
//! then drive SIP at the endpoint's address.

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;
use sip_net::UdpEndpoint;
use sip_txn::{IdGen, TransactionConfig, TransactionLayer};

use crate::cdr::CdrWriter;
use crate::config::B2buaConfig;
use crate::decision::CallDecisionEngine;
use crate::dispatch::PerCallDispatcher;
use crate::limiter::CallLimiter;
use crate::metrics::B2buaMetrics;
use crate::router::{self, RouterCtx};
use crate::rules::default_rules;
use crate::store::{BufferedTerminateWriter, CallState, CallStore};
use crate::timers::TimerService;

/// A running B2BUA worker. Holds the shared context; the router loop runs on a
/// spawned task that lives until the endpoint closes.
pub struct B2buaCore {
    ctx: Arc<RouterCtx>,
    metrics: B2buaMetrics,
    cdr: Arc<dyn CdrWriter>,
}

/// Wiring inputs for [`B2buaCore::spawn`].
pub struct B2buaDeps {
    pub config: B2buaConfig,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    pub cdr: Arc<dyn CdrWriter>,
    pub store: Arc<dyn CallStore>,
    pub clock: Clock,
    pub id_gen: Arc<IdGen>,
}

impl B2buaCore {
    /// Build over an already-bound endpoint and spawn the router loop.
    pub fn spawn(endpoint: Box<dyn UdpEndpoint>, deps: B2buaDeps) -> Self {
        let B2buaDeps {
            config,
            decision,
            limiter,
            cdr,
            store,
            clock,
            id_gen,
        } = deps;
        let metrics = B2buaMetrics::new();

        let parser: Arc<dyn SipParser + Send + Sync> = Arc::new(CustomParser::new());
        let (txn, txn_rx) = TransactionLayer::spawn(
            endpoint,
            parser,
            TransactionConfig {
                udp_queue_max: 256,
                id_gen: id_gen.clone(),
            },
        );
        let (timers, timer_rx) = TimerService::spawn(clock.clone());
        let terminate_writer = BufferedTerminateWriter::spawn(store.clone(), 1024);
        let state = CallState::new(store, terminate_writer, config.self_ordinal.clone(), metrics.clone());
        let dispatcher = PerCallDispatcher::new(
            config.event_dispatch_concurrency,
            config.per_call_queue_depth,
            config.per_call_queue_cap,
            metrics.clone(),
        );

        let ctx = Arc::new(RouterCtx {
            config,
            state,
            txn,
            timers,
            dispatcher,
            decision,
            limiter,
            cdr: cdr.clone(),
            id_gen,
            clock,
            rules: Arc::new(default_rules()),
            metrics: metrics.clone(),
        });

        tokio::spawn(router::run(ctx.clone(), txn_rx, timer_rx));

        Self { ctx, metrics, cdr }
    }

    pub fn metrics(&self) -> &B2buaMetrics {
        &self.metrics
    }

    pub fn cdr(&self) -> &Arc<dyn CdrWriter> {
        &self.cdr
    }

    /// Active call count (test/observability).
    pub fn active_calls(&self) -> usize {
        self.ctx.state.active_count()
    }
}
