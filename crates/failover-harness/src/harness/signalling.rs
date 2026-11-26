//! The signalling-fabric fault verbs of [`FailoverHarness`]: a node cut off
//! the SIP plane, or one whose every send lands late, while its process runs.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_net_testkit::{SendDelay, SendFault, SimulatedSignalingNetwork};

use super::FailoverHarness;

/// The SIP addresses the fabric cuts or holds.
#[derive(Default)]
pub(super) struct SignallingFaults {
    /// Cut off: every send from or to one fails at its endpoint, so the node
    /// is unreachable while its process keeps running and keeps firing its
    /// own timers.
    cut: Arc<Mutex<BTreeSet<SocketAddr>>>,
    /// Held: every send from one lands this much past the transit, so the
    /// node answers late while its process keeps up.
    slow: Arc<Mutex<BTreeMap<SocketAddr, Duration>>>,
}

impl SignallingFaults {
    /// The signalling fabric, 1 ms of transit (deterministic under a paused
    /// runtime), with the hooks these faults act through.
    pub(super) fn fabric() -> (SimulatedSignalingNetwork, Self) {
        let faults = Self::default();
        let cut = faults.cut.clone();
        let fault: SendFault = Arc::new(move |src: SocketAddr, dst: SocketAddr| {
            let cut = cut.lock().unwrap();
            (cut.contains(&src) || cut.contains(&dst))
                .then(|| format!("{src} is cut off the signalling fabric"))
        });
        let slow = faults.slow.clone();
        let hold: SendDelay = Arc::new(move |src: SocketAddr, _: SocketAddr, _: &[u8]| {
            slow.lock().unwrap().get(&src).copied()
        });
        let net = SimulatedSignalingNetwork::new(1).with_send_fault(fault).with_send_delay(hold);
        (net, faults)
    }
}

impl FailoverHarness {
    /// **Cut `addr` off the signalling fabric, both directions** — the node is
    /// unreachable on the SIP plane while its process keeps running, keeps its
    /// calls and keeps firing their timers. Every datagram it sends and every
    /// datagram addressed to it fails at the sending endpoint, so nothing
    /// crosses and nothing queues. The replication fabric is separate
    /// ([`partition`](Self::partition)). Marker.
    pub fn cut_signalling(&mut self, ordinal: &str, addr: SocketAddr) {
        self.sip_faults.cut.lock().unwrap().insert(addr);
        self.mark(ordinal, None, "sip-partition", &format!("{addr} unreachable on the SIP plane"));
    }

    /// **Hold every datagram `addr` sends by `by`** on the signalling fabric:
    /// its answers, its probe answers included, arrive that late while its
    /// process keeps up with its work, as a node starved of CPU answers. What
    /// it receives is not held. Lifted by
    /// [`restore_signalling`](Self::restore_signalling). Marker.
    pub fn slow_signalling(&mut self, ordinal: &str, addr: SocketAddr, by: Duration) {
        self.sip_faults.slow.lock().unwrap().insert(addr, by);
        self.mark(ordinal, None, "sip-slow", &format!("{addr} sends {by:?} late"));
    }

    /// Restore a signalling cut or hold. Marker.
    pub fn restore_signalling(&mut self, ordinal: &str, addr: SocketAddr) {
        self.sip_faults.cut.lock().unwrap().remove(&addr);
        self.sip_faults.slow.lock().unwrap().remove(&addr);
        self.mark(ordinal, None, "sip-heal", &format!("{addr} reachable again"));
    }
}
