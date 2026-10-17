//! The owner task's loop: one `select!` per turn over forgets, commands, due
//! timers, the safety-net sweep and inbound packets, each turn stamped on the
//! owner's [`LoopPulse`] from the instant it takes work until it waits again.

use std::sync::Arc;

use sip_net::{UdpEndpoint, UdpPacket};
use tokio::sync::mpsc;
use tokio_util::time::DelayQueue;

use crate::pulse::LoopPulse;
use crate::timers::{ms, TXN_SWEEP_INTERVAL};

use super::handle::{Command, Forget};
use super::owner::Owner;
use super::txn::Timer;

/// The next expired timer. Only ever awaited while `q` is non-empty — an empty
/// `DelayQueue` resolves `poll_expired` to `Ready(None)`, which would busy-spin
/// `select!`, so the call site guards with `if !timers.is_empty()`.
async fn next_expired(q: &mut DelayQueue<Timer>) -> Timer {
    std::future::poll_fn(|cx| q.poll_expired(cx))
        .await
        .expect("guarded by !is_empty()")
        .into_inner()
}

/// What one turn of the loop took.
enum Work {
    Forget(Forget),
    Command(Command),
    Timer(Timer),
    Sweep,
    Packet(UdpPacket),
}

/// Run the owner until every handle drops or the endpoint closes.
pub(super) async fn run(
    mut owner: Owner,
    endpoint: Box<dyn UdpEndpoint>,
    mut cmd_rx: mpsc::Receiver<Command>,
    mut forget_rx: mpsc::UnboundedReceiver<Forget>,
    pulse: Arc<LoopPulse>,
) {
    let endpoint: &dyn UdpEndpoint = endpoint.as_ref();
    let mut sweep = tokio::time::interval(ms(TXN_SWEEP_INTERVAL));
    // Coalesce missed ticks: after any owner stall the catch-up must be a single
    // pass, not a burst of back-to-back full-map scans (default `Burst`).
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first `interval` tick is immediate; skip it so the first sweep runs a
    // full interval after start.
    sweep.tick().await;

    loop {
        // `biased` polls the arms top-to-bottom and the INBOUND PACKET arm is LAST
        // on purpose: a sustained packet flood keeps `endpoint.recv()` ready every
        // iteration, so if it were polled first it would starve the timer wheel and
        // the safety-net sweep indefinitely — retransmits/timeouts/cleanups would
        // stall and the transaction maps + DelayQueue would grow unbounded exactly under
        // the overload the sweep exists to bound. Polling commands, due timers, and
        // the sweep ahead of new packets guarantees the internal machinery always
        // makes progress; the flood source (packets) can only be drained once
        // nothing else is pending. (cmd/timer can't themselves flood without
        // packets — the router only issues commands in response to events.)
        // A forget goes first: a retransmission read after it was posted then
        // meets no transaction and is admitted afresh.
        let work = tokio::select! {
            biased;
            Some(f) = forget_rx.recv() => Work::Forget(f),
            cmd = cmd_rx.recv() => match cmd {
                Some(c) => Work::Command(c),
                None => break, // all handles dropped → shut down
            },
            timer = next_expired(&mut owner.timers), if !owner.timers.is_empty() => {
                Work::Timer(timer)
            }
            _ = sweep.tick() => Work::Sweep,
            packet = endpoint.recv() => match packet {
                Some(p) => Work::Packet(p),
                None => break, // endpoint closed
            },
        };
        pulse.turning();
        match work {
            Work::Forget(f) => owner.forget_unanswered(&f.key, &f.call_id, &f.from_tag),
            Work::Command(c) => owner.handle_command(endpoint, c).await,
            Work::Timer(t) => owner.fire_timer(endpoint, t).await,
            Work::Sweep => owner.sweep(),
            Work::Packet(p) => owner.handle_packet(endpoint, p).await,
        }
        // End-of-turn: emit deferred CallQuiesced notices AFTER this turn's
        // protocol events, so the router never self-releases a takeover copy ahead
        // of the ACK/Timeout that cleared the call's last txn (ADR-0014 ordering).
        owner.flush_pending_quiesce();
        // Sample the DelayQueue depth (O(1)) so a timer/slab leak is visible even
        // when active_transactions is flat (the no-chaos RSS-climb localisation).
        owner
            .metrics
            .timer_queue_len
            .store(owner.timers.len(), std::sync::atomic::Ordering::Relaxed);
        owner
            .metrics
            .event_queue_deferred
            .store(owner.deferred_events.len(), std::sync::atomic::Ordering::Relaxed);
        pulse.waiting();
    }
}
