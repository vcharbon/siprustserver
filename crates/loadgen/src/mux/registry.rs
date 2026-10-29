//! One socket's routing state: the dialogs it demuxes by Call-ID, the call
//! slots it demuxes by token, and what a finished call leaves behind — its
//! Call-IDs held back, the INVITE transactions still owed an ACK, and the
//! answers a retransmitted request still draws.

use std::collections::{HashMap, VecDeque};

use tokio::time::Instant;

use super::late_answer::{HeldAnswers, LateReply};
use super::late_final::Held;
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
    released_invites: HashMap<String, Held>,
    /// The answers a released dialog's agent sent, held with its Call-ID: a
    /// retransmission of an answered request draws its final again (RFC 3261
    /// §17.2.2).
    released_answers: HashMap<String, HeldAnswers>,
}

impl SocketRegistry {
    /// Drop every key an endpoint owned: its Call-IDs are held back until
    /// `deadline` (with what `held` gives for each: the INVITE it still owes an
    /// ACK, the answers its agent sent), and it leaves its token slots (a slot
    /// goes with its last receiver; a claim that never fired is counted in
    /// `stats`).
    pub(in crate::mux) fn release_keys(
        &mut self,
        keys: Vec<Key>,
        mut held: impl FnMut(&str) -> (Option<Held>, Option<HeldAnswers>),
        deadline: Instant,
        stats: &MuxStats,
    ) {
        for key in keys {
            match key {
                Key::CallId(c) => {
                    self.by_call_id.remove(&c);
                    let (invite, answers) = held(&c);
                    if let Some(invite) = invite {
                        self.released_invites.insert(c.clone(), invite);
                    }
                    if let Some(answers) = answers {
                        self.released_answers.insert(c.clone(), answers);
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
        &mut self,
        call_id: &str,
        now: Instant,
    ) -> Option<&mut Held> {
        if !self.is_released(call_id, now) {
            return None;
        }
        self.released_invites.get_mut(call_id)
    }

    /// What the request `raw` on the released dialog `call_id` draws from its
    /// held answers, while its hold lasts at `now`.
    pub(in crate::mux) fn released_answer(
        &self,
        call_id: &str,
        raw: &[u8],
        now: Instant,
    ) -> LateReply {
        match self.released_answers.get(call_id) {
            Some(answers) if self.is_released(call_id, now) => answers.on_request(raw),
            _ => LateReply::NotHeld,
        }
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
                self.released_answers.remove(&call_id);
            }
        }
    }
}
