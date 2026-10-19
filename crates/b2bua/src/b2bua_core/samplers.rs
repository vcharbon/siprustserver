//! The worker's periodic samplers: the load sampler behind `X-Overload` and
//! the capacity gate, and the wall-clock divergence sampler. Each rides
//! `tokio::time::interval`, so a paused-clock test advances it with
//! `tokio::time::advance`; each owns no per-call state, so it needs no release
//! path, and a run that panics is restarted and counted
//! ([`crate::supervise`]).

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use tokio::task::JoinHandle;

use crate::capacity::Occupancy;
use crate::metrics::B2buaMetrics;
use crate::overload::OverloadSignal;
use crate::router::RouterCtx;
use crate::supervise::{self, CLOCK_DIVERGENCE, OVERLOAD_SAMPLER};

/// How often the divergence sampler reads the raw wall clock.
const DIVERGENCE_PERIOD: Duration = Duration::from_secs(30);

/// The divergence past which the sampler warns, once per rising edge.
const DIVERGENCE_WARN_MS: i64 = 500;

/// The worker-side load sampler: each tick reads the ELU/GC sampler and feeds
/// the EWMAs published on `X-Overload`, then samples the capacity gate (RSS
/// and the level the ingress brake reads).
pub(super) fn spawn_overload_sampler(
    overload: OverloadSignal,
    ctx: Arc<RouterCtx>,
) -> JoinHandle<()> {
    supervise::spawn(OVERLOAD_SAMPLER, move || {
        let (overload, ctx) = (overload.clone(), ctx.clone());
        async move {
            #[expect(clippy::disallowed_methods, reason = "SAMPLE_PERIOD is not zero")]
            let mut tick = tokio::time::interval(OverloadSignal::SAMPLE_PERIOD);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // skip the immediate first tick (TS first fire is +100 ms)
            loop {
                tick.tick().await;
                overload.sample();
                ctx.capacity.sample(Occupancy {
                    calls: ctx.unborn.with_live(|| ctx.state.active_count() as u64),
                    transactions: ctx.txn.metrics().active_transactions() as u64,
                });
            }
        }
    })
}

/// The clock-skew divergence sampler: every [`DIVERGENCE_PERIOD`] it compares
/// the raw system wall clock to this node's monotonic-anchored
/// `Clock::now_ms`, which does not follow a host NTP step, so the gap names
/// the event that skews cross-node replicated timer deadlines. It publishes
/// `clock_wall_divergence_ms` and warns on the rising edge past
/// [`DIVERGENCE_WARN_MS`]. Observability only: the clock is never re-anchored
/// (timestamps stay monotonic; the behavioural fix is the replication-boundary
/// re-anchor).
pub(super) fn spawn_clock_divergence(clock: Clock, metrics: B2buaMetrics) -> JoinHandle<()> {
    supervise::spawn(CLOCK_DIVERGENCE, move || {
        let (clock, metrics) = (clock.clone(), metrics.clone());
        async move {
            #[expect(clippy::disallowed_methods, reason = "DIVERGENCE_PERIOD is not zero")]
            let mut tick = tokio::time::interval(DIVERGENCE_PERIOD);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tick.tick().await; // skip the immediate first tick
            let mut warned_recently = false;
            loop {
                tick.tick().await;
                let divergence = clock.wall_divergence_ms(sip_clock::raw_system_wall_ms());
                metrics.set_clock_wall_divergence_ms(divergence);
                if divergence.saturating_abs() > DIVERGENCE_WARN_MS {
                    // A sustained step logs once, not every period.
                    if !warned_recently {
                        tracing::warn!(
                            divergence_ms = divergence,
                            "clock skew: wall-clock divergence (raw SystemTime - monotonic \
                             Clock::now_ms) — likely a host NTP step; cross-node replicated \
                             timer deadlines may skew. Fix is INFRA (slewing chrony + host \
                             kept awake), NOT the SUT."
                        );
                        warned_recently = true;
                    }
                } else {
                    warned_recently = false;
                }
            }
        }
    })
}
