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

/// The next expired timer; `None` once `q` is empty. Only awaited while `q` is
/// non-empty — an empty `DelayQueue` resolves `poll_expired` to `Ready(None)`
/// at once, which would busy-spin `select!`, so the call site guards with
/// `if !timers.is_empty()`.
async fn next_expired(q: &mut DelayQueue<Timer>) -> Option<Timer> {
    std::future::poll_fn(|cx| q.poll_expired(cx)).await.map(|e| e.into_inner())
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
///
/// Each turn polls its arms in order (`biased`): a forget first, so a
/// retransmission read after it was posted meets no transaction and is
/// admitted afresh; then commands, due timers and the sweep; inbound packets
/// last, so a flood that keeps the queue non-empty never starves the timer
/// wheel nor the sweep that bounds the maps under it. Every arm spends one
/// unit of the task's cooperative budget per item it yields, the packet arm
/// in [`sip_net::queue::PacketQueue::take`]: once a poll has spent the
/// budget every arm is pending, the poll ends, and the next one starts again
/// in this order. A command or a timer cannot flood without packets: the
/// router issues commands only in response to events.
pub(super) async fn run(
    mut owner: Owner,
    endpoint: Arc<dyn UdpEndpoint>,
    mut cmd_rx: mpsc::Receiver<Command>,
    mut forget_rx: mpsc::UnboundedReceiver<Forget>,
    pulse: Arc<LoopPulse>,
) {
    let endpoint: &dyn UdpEndpoint = endpoint.as_ref();
    #[expect(clippy::disallowed_methods, reason = "TXN_SWEEP_INTERVAL is not zero")]
    let mut sweep = tokio::time::interval(ms(TXN_SWEEP_INTERVAL));
    // Coalesce missed ticks: after any owner stall the catch-up must be a single
    // pass, not a burst of back-to-back full-map scans (default `Burst`).
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first `interval` tick is immediate; skip it so the first sweep runs a
    // full interval after start.
    sweep.tick().await;

    loop {
        let work = tokio::select! {
            biased;
            Some(f) = forget_rx.recv() => Work::Forget(f),
            cmd = cmd_rx.recv() => match cmd {
                Some(c) => Work::Command(c),
                None => break, // all handles dropped → shut down
            },
            Some(timer) = next_expired(&mut owner.timers), if !owner.timers.is_empty() => {
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
        owner.flush_owed_sends(endpoint).await;
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
