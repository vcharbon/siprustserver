//! The inbound path: one dispatcher per defined endpoint parses the routing
//! key, applies the per-call loss/retransmit state, and delivers to the right
//! call's inbox — or counts-and-drops. Plus the pending-slot reaper.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use scenario_harness::legpick::LegInfo;
use sip_message::message_helpers::is_invite_request_buffer;
use sip_message::sniff::call_id;
use sip_net::queue::PacketQueue;
use sip_net::{UdpEndpoint, UdpPacket};
use tokio::time::Instant;

use super::stats::{MuxStats, OrphanReason};
use super::{CallSlot, Delivery, Key, MuxSocket};

/// One socket's dispatcher: parse the routing key, deliver or count-and-drop.
/// Exits when the endpoint's inbox closes (fabric endpoint dropped) or the
/// owning [`MuxSocket`] is gone.
pub(super) async fn dispatch_loop(mux: std::sync::Weak<MuxSocket>, endpoint: Arc<dyn UdpEndpoint>) {
    while let Some(pkt) = endpoint.recv().await {
        let Some(mux) = mux.upgrade() else { return };
        route(&mux, &pkt.raw, pkt.src);
    }
}

/// Demux precedence: (1) known Call-ID — covers EVERY in-dialog datagram with
/// no token or R-URI cooperation; (2) correlation token — an initial INVITE
/// spawning a new leg of a known call; (3) orphan.
fn route(mux: &MuxSocket, raw: &[u8], src: SocketAddr) {
    let cid = call_id(raw);
    let mut g = mux.reg.lock().unwrap();

    // 1. Known dialog (Call-ID we minted or already promoted). Clone the
    //    `Delivery` out and RELEASE the registry lock before running the loss
    //    check + retransmit engine (which may send on the socket) — never hold
    //    `reg` across a send.
    if let Some(cid) = &cid {
        if let Some(d) = g.by_call_id.get(cid) {
            let d = d.clone();
            drop(g);
            handle_inbound(mux, &d, raw, src);
            return;
        }
    }
    // 2. A new leg of a known call: an INITIAL INVITE whose Call-ID we have not
    //    seen, carrying our per-call token. Only an initial INVITE may spawn a
    //    leg (an in-dialog request with an unknown Call-ID is a stray, never a
    //    new dialog). The token entry is NON-consuming: it persists for the
    //    call's lifetime so re-routes / multi-REFER / re-REFER (further legs of
    //    the same call on this socket) each promote their own dialog.
    if is_invite_request_buffer(raw) {
        let Some(tok) = mux.correlation.token(raw) else {
            mux.stats.orphan(OrphanReason::NoHeader, raw);
            return;
        };
        let Some(slot) = g.by_token.get_mut(&tok) else {
            mux.stats.orphan(OrphanReason::UnknownToken, raw);
            return;
        };
        // Receiver selection: the single-receiver socket delivers directly; a
        // shared socket asks the scenario-owned picker (handed the parsed leg).
        // The mux itself reads nothing but the call token — leg routing is the
        // scenario's to own.
        let idx = match slot.receivers.len() {
            0 => {
                mux.stats.orphan(OrphanReason::NoRoute, raw);
                return;
            }
            1 => 0,
            _ => match &slot.picker {
                Some(pick) => {
                    // The picker runs while we hold `mux.reg`; isolate it under
                    // `catch_unwind` so a panicking scenario callback degrades to
                    // a `no_route` orphan instead of POISONING the socket's
                    // registry mutex (which would cascade every subsequent
                    // route/bind/drop on this endpoint). It must also not re-enter
                    // the mux (it would self-deadlock on `reg`).
                    let picked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        pick(&LegInfo::new(raw))
                    }));
                    match picked.ok().and_then(|want| slot.receivers.iter().position(|r| r.label == want)) {
                        Some(i) => i,
                        None => {
                            mux.stats.orphan(OrphanReason::NoRoute, raw);
                            tap_unrouted(mux, slot, raw, src);
                            return;
                        }
                    }
                }
                None => {
                    // Several receivers but no picker to disambiguate — a
                    // scenario bug, not a silent first-wins.
                    mux.stats.orphan(OrphanReason::NoRoute, raw);
                    tap_unrouted(mux, slot, raw, src);
                    return;
                }
            },
        };
        slot.arrived = true;
        // Snapshot this receiver's delivery state, then (under the same lock)
        // promote token→Call-ID so in-dialog traffic demuxes directly.
        let delivery = {
            let recv = &slot.receivers[idx];
            Delivery { queue: recv.queue.clone(), drop: recv.drop.clone(), txns: recv.txns.clone() }
        };
        if let Some(cid) = &cid {
            slot.receivers[idx].keyset.lock().unwrap().push(Key::CallId(cid.clone()));
            g.by_call_id.insert(cid.clone(), delivery.clone());
        }
        drop(g);
        handle_inbound(mux, &delivery, raw, src);
        return;
    }
    // 3. Not a known dialog and not an initial INVITE → a late straggler.
    mux.stats.orphan(OrphanReason::Stray, raw);
}

/// Hand one resolved-to-a-call datagram to the app: apply simulated inbound loss
/// (above the retransmit engine, so a lost datagram is truly gone and the peer's
/// retransmit re-delivers it fresh), then let the retransmit engine dedup /
/// stop-resenders / re-answer; a datagram the engine ABSORBS (a duplicate the app
/// must not see) is not enqueued.
fn handle_inbound(mux: &MuxSocket, d: &Delivery, raw: &[u8], src: SocketAddr) {
    if d.drop.drops_inbound(raw) {
        mux.stats.dropped_in.fetch_add(1, Ordering::Relaxed);
        // A recorded (sampled) call still shows the arrival on its ladder,
        // tagged as modeled loss — the RFC audit filters it out.
        tap_discard(mux, d, raw, src, sip_net::RecvDisposition::LossModel);
        return;
    }
    if let Some(txns) = &d.txns {
        if !txns.on_inbound(raw, src) {
            // duplicate absorbed (engine did any re-ACK / re-answer)
            tap_discard(mux, d, raw, src, sip_net::RecvDisposition::AbsorbedRetransmit);
            return;
        }
    }
    let arrival_ms = mux.clock.now_ms().max(0) as u64;
    deliver(&mux.stats, &d.queue, UdpPacket { raw: raw.to_vec(), src, arrival_ms });
}

/// Report a demuxed-but-discarded datagram to a recorded inbox's delivery tap
/// (no-op — not even an allocation — on the unsampled path).
fn tap_discard(mux: &MuxSocket, d: &Delivery, raw: &[u8], src: SocketAddr, disp: sip_net::RecvDisposition) {
    if let Some(tap) = d.queue.recv_tap() {
        let arrival_ms = mux.clock.now_ms().max(0) as u64;
        tap(&UdpPacket { raw: raw.to_vec(), src, arrival_ms }, disp);
    }
}

/// A datagram that CORRELATED to the call (its token matched this slot) but
/// no live logical endpoint accepted it (picker miss / undisambiguated
/// receivers): on a recorded call, still record the arrival — tagged
/// `Unrouted`, rendered on the `ip:port#noendpoint` sub-lane. Uncorrelated
/// datagrams never reach here and stay counter-only, so cross-call noise
/// cannot contaminate a sampled trace.
fn tap_unrouted(mux: &MuxSocket, slot: &CallSlot, raw: &[u8], src: SocketAddr) {
    let Some(tap) = slot.receivers.first().and_then(|r| r.queue.recv_tap()) else {
        return;
    };
    let arrival_ms = mux.clock.now_ms().max(0) as u64;
    tap(
        &UdpPacket { raw: raw.to_vec(), src, arrival_ms },
        sip_net::RecvDisposition::Unrouted,
    );
}

fn deliver(stats: &MuxStats, q: &PacketQueue, pkt: UdpPacket) {
    if q.offer(pkt) {
        stats.delivered.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.inbox_drop.fetch_add(1, Ordering::Relaxed);
    }
}

/// Periodic sweep of pending callee legs whose INVITE never arrived. A slot that
/// has seen at least one leg (`arrived`) is left alone — a live call may outlast
/// the pending deadline; its receivers are released on agent `Drop`, not here.
pub(super) async fn reap_loop(sockets: Vec<Arc<MuxSocket>>, stats: Arc<MuxStats>, ttl: Duration) {
    let mut tick = tokio::time::interval(ttl.max(Duration::from_secs(5)));
    loop {
        tick.tick().await;
        let now = Instant::now();
        for mux in &sockets {
            let mut g = mux.reg.lock().unwrap();
            let expired: Vec<String> = g
                .by_token
                .iter()
                .filter(|(_, s)| !s.arrived && s.deadline <= now)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired {
                if let Some(s) = g.by_token.remove(&k) {
                    for r in &s.receivers {
                        r.queue.close();
                    }
                    stats.pending_expired.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}
