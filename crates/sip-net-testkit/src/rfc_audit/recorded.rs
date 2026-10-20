//! Which parties the recording holds a lane for: the one fact the live
//! adapter needs to report what a sender minted where its own lane is absent.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use layer_harness::{LaneKey, Stamped};

use crate::contracts::SignalingNetworkEvent;
use crate::report::lane_addr;

/// The parties the recording holds a lane for. A bind is known by its key, by
/// the socket address the key names, and by the address its datagrams arrive
/// from at another recorded bind — a bind asked at port 0 is recorded under the
/// address it asked for and sends from the one it was given. A bind a consumer
/// scoped out of the view is recorded all the same.
pub(crate) struct Recorded {
    keys: HashSet<LaneKey>,
    addrs: HashSet<SocketAddr>,
}

impl Recorded {
    pub(crate) fn of(
        events: &[Stamped<SignalingNetworkEvent>],
        scoped_out: &HashSet<LaneKey>,
    ) -> Recorded {
        let mut keys = scoped_out.clone();
        let mut sends: HashMap<&[u8], &LaneKey> = HashMap::new();
        for s in events {
            match &s.event {
                SignalingNetworkEvent::BindAcquire { bind_key, .. } => {
                    keys.insert(bind_key.clone());
                }
                SignalingNetworkEvent::SendCalled { bind_key, msg, .. } => {
                    keys.insert(bind_key.clone());
                    sends.entry(msg.as_slice()).or_insert(bind_key);
                }
                SignalingNetworkEvent::RecvItem { bind_key, .. } => {
                    keys.insert(bind_key.clone());
                }
                _ => {}
            }
        }
        let mut addrs: HashSet<SocketAddr> = keys.iter().filter_map(|k| lane_addr(k)).collect();
        for s in events {
            if let SignalingNetworkEvent::RecvItem { bind_key, packet, .. } = &s.event {
                if sends.get(packet.raw.as_slice()).is_some_and(|from| *from != bind_key) {
                    addrs.insert(packet.src);
                }
            }
        }
        Recorded { keys, addrs }
    }

    /// Whether `party` — a bind key or a socket address — is a recorded bind.
    pub(crate) fn holds(&self, party: &str) -> bool {
        self.keys.contains(party) || lane_addr(party).is_some_and(|a| self.addrs.contains(&a))
    }
}
