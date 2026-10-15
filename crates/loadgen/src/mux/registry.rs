//! One socket's routing state: the dialogs it demuxes by Call-ID, the call
//! slots it demuxes by token, and what a finished call leaves behind — its
//! Call-IDs held back, and the INVITE transactions still owed an ACK.

use std::collections::{HashMap, VecDeque};

use tokio::time::Instant;

use super::late_final::SentInvite;
use super::stats::MuxStats;
use super::{CallSlot, Delivery, Key};

#[derive(Default)]
pub(in crate::mux) struct SocketRegistry {
    pub(in crate::mux) by_call_id: HashMap<String, Delivery>,
    pub(in crate::mux) by_token: HashMap<String, CallSlot>,
    /// Call-IDs released by finished calls, until their hold deadline.
    released: HashMap<String, Instant>,
    /// The same holds in deadline order, so expiry pops only what expired.
    released_order: VecDeque<(Instant, String)>,
    /// The INVITE a released caller dialog last sent, held with its Call-ID:
    /// a non-2xx final to it is still ACKed (RFC 3261 §17.1.1.3).
    released_invites: HashMap<String, SentInvite>,
}

impl SocketRegistry {
    /// Drop every key an endpoint owned: its Call-IDs are held back until
    /// `deadline` (with the INVITE each still owes an ACK, from `invites`), and
    /// it leaves its token slots (a slot goes with its last receiver; a claim
    /// that never fired is counted in `stats`).
    pub(in crate::mux) fn release_keys(
        &mut self,
        keys: Vec<Key>,
        mut invites: impl FnMut(&str) -> Option<SentInvite>,
        deadline: Instant,
        stats: &MuxStats,
    ) {
        for key in keys {
            match key {
                Key::CallId(c) => {
                    self.by_call_id.remove(&c);
                    if let Some(invite) = invites(&c) {
                        self.released_invites.insert(c.clone(), invite);
                    }
                    self.released.insert(c.clone(), deadline);
                    self.released_order.push_back((deadline, c));
                }
                Key::Token { token, label } => self.leave_slot(&token, &label, stats),
            }
        }
    }

    /// Remove only `label`'s receiver from a possibly-shared slot; drop the
    /// slot once its last receiver leaves.
    fn leave_slot(&mut self, token: &str, label: &str, stats: &MuxStats) {
        let Some(slot) = self.by_token.get_mut(token) else { return };
        if let Some(pos) = slot.receivers.iter().position(|r| r.label == label) {
            let r = slot.receivers.remove(pos);
            if r.claim.is_some() && !r.claimed {
                stats.claim_unfired.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if slot.receivers.is_empty() {
            self.by_token.remove(token);
        }
    }

    /// Whether `call_id` is held back at `now`.
    pub(in crate::mux) fn is_released(&self, call_id: &str, now: Instant) -> bool {
        self.released.get(call_id).is_some_and(|deadline| *deadline > now)
    }

    /// The INVITE a released dialog last sent, while its hold lasts at `now`.
    pub(in crate::mux) fn released_invite(
        &self,
        call_id: &str,
        now: Instant,
    ) -> Option<&SentInvite> {
        self.is_released(call_id, now).then(|| self.released_invites.get(call_id)).flatten()
    }

    /// Drop the holds that expired by `now`, touching only those.
    pub(in crate::mux) fn expire_released(&mut self, now: Instant) {
        while let Some((deadline, _)) = self.released_order.front() {
            if *deadline > now {
                break;
            }
            let (_, call_id) = self.released_order.pop_front().expect("front exists");
            if self.released.get(&call_id).is_some_and(|d| *d <= now) {
                self.released.remove(&call_id);
                self.released_invites.remove(&call_id);
            }
        }
    }
}
