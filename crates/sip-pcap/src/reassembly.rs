//! IPv4/IPv6 fragment reassembly. Lossy captures are the norm — the ring
//! rotates and the BPF filter sees only some ports — so this file's whole
//! concern is turning fragments into complete IP payloads *without ever
//! inventing bytes*.
//!
//! Invariants (all breaches counted in [`DecodeStats`], never silent):
//! - a datagram is emitted ONLY when every byte `0..total_len` is covered;
//!   a hole (missed fragment) is never padded or passed to the SIP stack,
//! - pending reassemblies are bounded ([`MAX_PENDING_REASSEMBLIES`], oldest
//!   evicted first) and size-capped ([`MAX_REASSEMBLED_LEN`]),
//! - pending entries expire after [`REASSEMBLY_TTL_US`] of *capture* time
//!   (pcap timestamps, not wall clock, so offline analysis behaves the same
//!   as live capture would).

use std::collections::HashMap;
use std::net::IpAddr;

use crate::DecodeStats;

/// Max concurrent in-progress reassemblies before the oldest is evicted.
pub const MAX_PENDING_REASSEMBLIES: usize = 4096;
/// Hard cap on a reassembled IP datagram (RFC limit is 64 KiB; SIP is far
/// smaller — anything bigger is garbage or an attack capture).
pub const MAX_REASSEMBLED_LEN: usize = 128 * 1024;
/// A pending reassembly older than this (in capture time) is dropped — its
/// missing fragment was never captured.
pub const REASSEMBLY_TTL_US: u64 = 30_000_000;

/// Reassembly key: RFC 791 (src, dst, protocol, identification); IPv6 uses
/// the fragment-header ident with proto folded in the same way.
#[derive(Hash, PartialEq, Eq, Clone)]
pub struct FragKey {
    pub src: IpAddr,
    pub dst: IpAddr,
    pub proto: u8,
    pub ident: u32,
}

struct FragBuf {
    first_seen_us: u64,
    /// (offset, bytes) pieces as captured, in arrival order. Overlaps are
    /// resolved last-writer-wins at assembly time.
    pieces: Vec<(usize, Vec<u8>)>,
    /// Total payload length, known once the MF=0 fragment arrives.
    total_len: Option<usize>,
    bytes_buffered: usize,
}

impl FragBuf {
    /// Assemble iff every byte `0..total` is covered. Holes → `None`.
    fn try_assemble(&self) -> Option<Vec<u8>> {
        let total = self.total_len?;
        if total > MAX_REASSEMBLED_LEN {
            return None;
        }
        let mut buf = vec![0u8; total];
        let mut covered = vec![false; total];
        for (off, bytes) in &self.pieces {
            let end = off.checked_add(bytes.len())?;
            if end > total {
                return None; // fragment past the declared end — corrupt
            }
            buf[*off..end].copy_from_slice(bytes);
            for c in &mut covered[*off..end] {
                *c = true;
            }
        }
        covered.iter().all(|c| *c).then_some(buf)
    }
}

/// Outcome of feeding one fragment to the reassembler.
pub enum FragOutcome {
    /// Complete IP payload.
    Complete(Vec<u8>),
    /// Buffered; more fragments needed.
    Pending,
}

/// Fragment reassembler shared across every record of a run (a datagram's
/// fragments may straddle two ring files — feed files in time order).
pub struct Reassembler {
    pending: HashMap<FragKey, FragBuf>,
    /// Insertion order for oldest-first eviction (coarse; entries may already
    /// be gone when popped — that's fine, we skip them).
    order: Vec<FragKey>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self { pending: HashMap::new(), order: Vec::new() }
    }

    /// Fragments still incomplete — counted as dropped at end of input.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn push(
        &mut self,
        stats: &mut DecodeStats,
        ts_us: u64,
        key: FragKey,
        frag_off: usize,
        more_fragments: bool,
        bytes: &[u8],
    ) -> FragOutcome {
        stats.fragments += 1;
        self.expire(stats, ts_us);

        if !self.pending.contains_key(&key) {
            self.order.push(key.clone());
            self.pending.insert(
                key.clone(),
                FragBuf { first_seen_us: ts_us, pieces: Vec::new(), total_len: None, bytes_buffered: 0 },
            );
        }
        let (over_cap, assembled) = {
            let entry = self.pending.get_mut(&key).expect("just inserted");
            entry.bytes_buffered += bytes.len();
            entry.pieces.push((frag_off, bytes.to_vec()));
            if !more_fragments {
                entry.total_len = Some(frag_off + bytes.len());
            }
            // Size guard: a runaway (or hostile) fragment stream is dropped whole.
            let over = entry.bytes_buffered > MAX_REASSEMBLED_LEN;
            (over, if over { None } else { entry.try_assemble() })
        };
        if over_cap {
            self.pending.remove(&key);
            stats.frag_dropped += 1;
            return FragOutcome::Pending;
        }
        if let Some(assembled) = assembled {
            self.pending.remove(&key);
            stats.reassembled += 1;
            return FragOutcome::Complete(assembled);
        }
        // Table-size guard: evict oldest pending entries beyond the cap.
        while self.pending.len() > MAX_PENDING_REASSEMBLIES {
            match self.order.first().cloned() {
                Some(oldest) => {
                    self.order.remove(0);
                    if self.pending.remove(&oldest).is_some() {
                        stats.frag_dropped += 1;
                    }
                }
                None => break,
            }
        }
        FragOutcome::Pending
    }

    /// Drop pending entries whose missing fragments were never captured.
    fn expire(&mut self, stats: &mut DecodeStats, now_us: u64) {
        if self.pending.is_empty() {
            return;
        }
        let before = self.pending.len();
        self.pending.retain(|_, b| now_us.saturating_sub(b.first_seen_us) <= REASSEMBLY_TTL_US);
        stats.frag_dropped += (before - self.pending.len()) as u64;
        if self.pending.is_empty() {
            self.order.clear();
        }
    }
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}
