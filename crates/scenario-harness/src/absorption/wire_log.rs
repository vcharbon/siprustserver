//! The arrival-order record both views project from: every sighted datagram
//! with the tag [`Absorption::sight`](super::Absorption::sight) gave it.

use std::sync::Mutex;

use super::{SeenBy, Sighting};

/// One sighted datagram: its bytes, its arrival position, and which view it
/// belongs to.
#[derive(Debug, Clone)]
pub struct WireEntry {
    /// Arrival position on this endpoint, from 0, gaps-free — the wire view IS
    /// this order.
    pub seq: usize,
    pub raw: Vec<u8>,
    pub sighting: Sighting,
}

impl WireEntry {
    pub fn seen_by(&self) -> SeenBy {
        self.sighting.seen_by()
    }

    /// Whether this datagram is a byte-identical repeat of an earlier one on
    /// the same transaction — the unit a retransmission ladder counts.
    pub fn is_repeat(&self) -> bool {
        self.sighting.repeat
    }

    /// The start-line, for a ladder assertion that reads as the wire does.
    pub fn start_line(&self) -> String {
        String::from_utf8_lossy(&self.raw).split("\r\n").next().unwrap_or_default().to_string()
    }
}

#[derive(Default)]
pub(super) struct WireLog {
    entries: Mutex<Vec<WireEntry>>,
}

impl WireLog {
    pub(super) fn push(&self, raw: &[u8], sighting: Sighting) {
        let mut entries = self.entries.lock().unwrap();
        let seq = entries.len();
        entries.push(WireEntry { seq, raw: raw.to_vec(), sighting });
    }

    pub(super) fn entries(&self) -> Vec<WireEntry> {
        self.entries.lock().unwrap().clone()
    }
}
