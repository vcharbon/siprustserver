//! One datagram as the mux's stores and watchers see it: its bytes, its
//! identity sniffed once ([`identity`]), and the one shared copy that every
//! store keeping it holds, made on first demand. A datagram crossing an
//! endpoint is sniffed into one [`Datagram`] and handed to each of them.

use std::cell::OnceCell;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use sip_message::sniff::{identity, Identity};

/// One datagram, sniffed.
pub struct Datagram<'a> {
    raw: &'a [u8],
    id: Identity<'a>,
    shared: OnceCell<Arc<[u8]>>,
}

impl<'a> Datagram<'a> {
    /// `raw`, its identity read.
    pub fn sniff(raw: &'a [u8]) -> Self {
        Self { raw, id: identity(raw), shared: OnceCell::new() }
    }

    /// The datagram's bytes.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    /// The datagram's identity.
    pub fn id(&self) -> &Identity<'a> {
        &self.id
    }

    /// Whether the datagram is a response (its status line read).
    pub fn is_response(&self) -> bool {
        self.id.status.is_some()
    }

    /// The final status of a response at or above 200.
    pub(in crate::mux) fn final_status(&self) -> Option<u16> {
        self.id.status.filter(|s| *s >= 200)
    }

    /// The copy of the bytes a store keeps: one allocation, shared by every
    /// store that asks.
    pub(in crate::mux) fn shared(&self) -> Arc<[u8]> {
        self.shared.get_or_init(|| Arc::from(self.raw)).clone()
    }
}

/// A store's key for the datagram fields `parts`: a 64-bit hash, so the store
/// copies none of their text. Two of one endpoint's few hundred keys collide
/// with a chance of the order of 1e-15.
pub(in crate::mux) fn key_of(parts: impl Hash) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}
