//! A peer's copies of one trigger, answered on the RFC schedule (ADR-0063
//! §1): the copies up to the re-sends of the ladder the peer re-sends the
//! trigger on are answered at any pace, a later one only T1 after the last
//! answered one, and none later than 64·T1 after the trigger's first arrival.
//! A server transaction keeps the record of its request and of a CANCEL of
//! it, a client INVITE transaction one per To-tag of its finals; a server
//! transaction's records outlive it in its Call-ID's entry
//! (`layer::call_id_budget`), so a copy that would re-open it continues the
//! count.

use sip_message::Method;
use sip_retransmit::{Class, COPY_WINDOW};
use tokio::time::Instant;

use super::txn::MAX_DIALOGS;

/// The copies of one trigger: when it first arrived, how many copies
/// followed, when the last one arrived (absorbed included) and when the last
/// answered one did.
#[derive(Debug, Clone, Copy)]
pub(super) struct Copies {
    class: Class,
    first: Instant,
    copies: u32,
    last: Instant,
    last_answered: Instant,
}

impl Copies {
    /// The trigger's first arrival at `now`, which its peer re-sends on
    /// `class`'s ladder.
    pub(super) fn first(class: Class, now: Instant) -> Self {
        Self { class, first: now, copies: 0, last: now, last_answered: now }
    }

    /// Count a copy arriving at `now`; whether it is answered: inside the
    /// 64·T1 of the first arrival and owed on the class's ladder
    /// ([`Class::owes_copy`]).
    pub(super) fn copy(&mut self, now: Instant) -> bool {
        self.copies = self.copies.saturating_add(1);
        self.last = now;
        let since_answered = now.saturating_duration_since(self.last_answered);
        let owed = now.saturating_duration_since(self.first) < COPY_WINDOW
            && self.class.owes_copy(self.copies, Some(since_answered));
        if owed {
            self.last_answered = now;
        }
        owed
    }

    /// Whether the record is over at `now`: 64·T1 passed since its last
    /// copy, the first arrival counting as one.
    pub(super) fn lapsed(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last) >= COPY_WINDOW
    }

    /// The trigger's first arrival: the record's identity over its life.
    pub(super) fn first_arrival(&self) -> Instant {
        self.first
    }

    /// The last copy's arrival, the first arrival when none followed.
    pub(super) fn last_copy(&self) -> Instant {
        self.last
    }
}

/// The class whose ladder a peer re-sends a request of `method` on: Timer A
/// for an INVITE (RFC 3261 §17.1.1.2), the CANCEL's own Timer E, Timer E for
/// any other request (§17.1.2.2).
pub(super) fn request_class(method: &Method) -> Class {
    match method {
        Method::Invite => Class::InviteClient,
        Method::Cancel => Class::CancelClient,
        _ => Class::NonInviteClient,
    }
}

/// The copies of a client INVITE transaction's finals, one record per
/// To-tag (a fork's dialog, RFC 3261 §12.1.2), at most [`MAX_DIALOGS`]: the
/// To-tags past them share one record.
#[derive(Debug, Default)]
pub(super) struct FinalCopies {
    tags: Vec<(Option<String>, Copies)>,
    past: Option<Copies>,
}

impl FinalCopies {
    /// A final under `to_tag` arriving at `now`, re-sent on `class`'s ladder:
    /// whether it is answered. The first arrival of its To-tag is; a copy is
    /// as [`Copies::copy`] says.
    pub(super) fn arrival(&mut self, to_tag: Option<&str>, class: Class, now: Instant) -> bool {
        if let Some((_, copies)) = self.tags.iter_mut().find(|(tag, _)| tag.as_deref() == to_tag) {
            return copies.copy(now);
        }
        if self.tags.len() < MAX_DIALOGS {
            self.tags.reserve_exact(1);
            self.tags.push((to_tag.map(str::to_string), Copies::first(class, now)));
            return true;
        }
        match self.past.as_mut() {
            Some(copies) => copies.copy(now),
            None => {
                self.past = Some(Copies::first(class, now));
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sip_retransmit::timers::T1;

    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    /// The copies up to the ladder's re-sends are answered at once, the next
    /// only T1 after the last answered one.
    #[tokio::test(start_paused = true)]
    async fn the_ladders_re_sends_are_answered_at_any_pace_then_one_per_t1() {
        let now = Instant::now();
        let mut copies = Copies::first(Class::InviteClient, now);
        let answered = (0..15).filter(|_| copies.copy(now)).count();
        assert_eq!(answered, 6, "Timer A's re-sends");
        assert!(!copies.copy(now + ms(T1 - 1)), "inside T1 of the last answered");
        assert!(copies.copy(now + ms(T1)), "T1 after it");
    }

    /// No copy later than 64·T1 after the first arrival is answered, and each
    /// one it absorbs keeps the record 64·T1 longer.
    #[tokio::test(start_paused = true)]
    async fn no_copy_past_64_t1_of_the_first_arrival_is_answered() {
        let now = Instant::now();
        let mut copies = Copies::first(Class::NonInviteClient, now);
        assert!(copies.copy(now + COPY_WINDOW - ms(1)), "inside the window");
        assert!(!copies.copy(now + COPY_WINDOW), "at its end");
        assert!(!copies.lapsed(now + COPY_WINDOW * 2 - ms(1)), "kept by the absorbed copy");
        assert!(copies.lapsed(now + COPY_WINDOW * 2), "64·T1 after it");
    }

    /// A client transaction's final re-sent past 64·T1 of its first arrival
    /// draws nothing, under each To-tag.
    #[tokio::test(start_paused = true)]
    async fn a_final_copied_past_64_t1_of_its_first_arrival_draws_nothing() {
        let now = Instant::now();
        let mut finals = FinalCopies::default();
        assert!(finals.arrival(Some("fork-a"), Class::Final2xx, now), "first arrival");
        assert!(finals.arrival(Some("fork-a"), Class::Final2xx, now + ms(T1)), "a copy");
        let late = now + COPY_WINDOW;
        assert!(!finals.arrival(Some("fork-a"), Class::Final2xx, late), "past 64·T1");
        assert!(finals.arrival(Some("fork-b"), Class::Final2xx, late), "another fork's first");
    }

    /// The To-tags past [`MAX_DIALOGS`] share one count: two of them copied
    /// at once draw one first arrival and the ladder's re-sends together.
    #[tokio::test(start_paused = true)]
    async fn the_to_tags_past_the_dialog_bound_share_one_count() {
        let now = Instant::now();
        let mut finals = FinalCopies::default();
        for fork in 0..MAX_DIALOGS {
            assert!(finals.arrival(Some(&format!("fork-{fork}")), Class::Final2xx, now));
        }
        let answered = (0..15)
            .flat_map(|_| ["past-a", "past-b"])
            .filter(|tag| finals.arrival(Some(tag), Class::Final2xx, now))
            .count();
        assert_eq!(answered, 1 + 10, "one first arrival and the 2xx ladder's re-sends");
    }
}
