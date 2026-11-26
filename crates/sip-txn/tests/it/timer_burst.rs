//! A burst of timers due at one instant: the owner fires them a cooperative
//! budget per task poll, so the burst never holds its thread for its whole
//! length.
//!
//! The runtime is the paused current-thread one, its poll hooks reading how
//! many transactions each task poll ended: the poll watchdog's reading of a
//! poll, in timers instead of CPU time.

use crate::common;
use common::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use sip_message::generators::generate_response;
use sip_message::SipMessage;
use sip_txn::timers::TIMER_J;
use sip_txn::{TransactionEvent, TransactionMetrics};

/// The transactions whose Timer J comes due at one instant.
const BURST: usize = 600;
/// Tokio's cooperative budget: the ready operations one task poll completes
/// before its next ready one returns pending.
const COOP_BUDGET: usize = 128;
/// The most timers one poll fires: a budget, give or take the expiry the
/// timer driver fired before the poll began.
const PER_POLL: usize = 2 * COOP_BUDGET;

/// The most transactions one task poll ended, read by the runtime's poll
/// hooks.
#[derive(Clone, Default)]
struct WidestPoll {
    metrics: Arc<OnceLock<TransactionMetrics>>,
    at_entry: Arc<AtomicUsize>,
    widest: Arc<AtomicUsize>,
}

impl WidestPoll {
    fn active(&self) -> usize {
        self.metrics.get().map_or(0, TransactionMetrics::active_transactions)
    }

    fn runtime(&self) -> tokio::runtime::Runtime {
        let (before, after) = (self.clone(), self.clone());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .on_before_task_poll(move |_| before.at_entry.store(before.active(), Ordering::SeqCst))
            .on_after_task_poll(move |_| {
                let ended = after.at_entry.load(Ordering::SeqCst).saturating_sub(after.active());
                after.widest.fetch_max(ended, Ordering::SeqCst);
            })
            .build()
            .expect("the runtime builds")
    }
}

/// RFC 3261 §17.2.2: each non-INVITE server transaction answered at one
/// instant ends on its Timer J, all due at the same instant.
#[test]
fn a_burst_of_due_timers_ends_the_owners_poll_every_budget() {
    let widest = WidestPoll::default();
    widest.runtime().block_on(async {
        let mut stack = Stack::build(1, BURST, BURST).await;
        let _ = widest.metrics.set(stack.txn.metrics().clone());
        for n in 0..BURST {
            let (branch, call_id) = (format!("z9hG4bK-burst-{n}"), format!("burst-{n}@unit"));
            stack.inject(&inbound_request("MESSAGE", &branch, &call_id, None)).await;
        }
        elapse_ms(20).await;
        let requests: Vec<_> = stack
            .drain_events()
            .into_iter()
            .filter_map(|event| match event {
                TransactionEvent::Message { message, .. } => match *message {
                    SipMessage::Request(request) => Some(request),
                    SipMessage::Response(_) => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), BURST, "every MESSAGE reaches the TU");
        for request in &requests {
            let ok = generate_response(request, 200, "OK", &Default::default()).expect("a 200");
            stack.txn.send_response(ok, addr(PEER)).await.expect("the 200 is sent");
        }
        assert_eq!(stack.txn.metrics().active_transactions(), BURST, "each one Completed");

        elapse_ms(TIMER_J + 100).await;
        assert_eq!(stack.txn.metrics().active_transactions(), 0, "Timer J ends every one");
    });
    let widest = widest.widest.load(Ordering::SeqCst);
    assert!(
        widest <= PER_POLL,
        "no poll of the owner fires much more than a budget of timers ({widest} of {BURST})"
    );
}
