//! The 2xx answers to OPTIONS an endpoint sent onward, read so a 2xx it
//! answers can be told relayed from its own (RFC 3261 §11.2, §16.7).
//!
//! An endpoint that took an OPTIONS, asked onward as a proxy or a B2BUA does,
//! and answered with the 2xx it took back carries the far UAS's description of
//! itself: a bare far 2xx relayed bare is the far UAS's miss, judged where the
//! far UAS sent it. The two legs are tied by order alone, whatever Call-ID the
//! far side wears, first in, first out, as a 2xx is forwarded when it arrives
//! (§16.7): an answer pairs with the FIRST far 2xx not yet paired that its
//! endpoint took before the answer left, to an OPTIONS it sent no earlier than
//! it took the one answered, and a far 2xx pairs with one answer at most. An
//! OPTIONS the endpoint sends on its own account inside that window is not
//! told apart from a forwarded one.

use std::collections::BTreeSet;

use crate::wire::{Kind, Msg};

use super::super::sides::Sides;
use super::carries_any;

/// One 2xx to an OPTIONS of the view, placed by the request it answers.
struct FarAnswer<'m> {
    /// The 2xx's index in the view.
    at: usize,
    /// When the OPTIONS it answers was sent.
    asked_us: u64,
    /// The socket that sent that OPTIONS.
    asked_by: &'m str,
    /// It carries none of the headers §11.2 asks of a 2xx to OPTIONS.
    bare: bool,
}

/// The far 2xx answers of a view, each paired with one answer at most.
pub(super) struct OnwardAnswers<'m> {
    answers: Vec<FarAnswer<'m>>,
    paired: BTreeSet<usize>,
}

impl<'m> OnwardAnswers<'m> {
    /// Every first 2xx to an OPTIONS in `msgs` whose request `sides` names.
    pub(super) fn of(msgs: &'m [Msg], sides: &Sides<'m>) -> Self {
        let first_2xx_to_options = |m: &Msg| {
            !m.repeat
                && matches!(m.kind, Kind::Response { status } if (200..300).contains(&status))
                && m.cseq_method.eq_ignore_ascii_case("OPTIONS")
        };
        let answers = msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| first_2xx_to_options(m))
            .filter_map(|(at, m)| {
                let request = sides.request_of(m)?;
                let bare = m.head.as_deref().is_some_and(|head| !carries_any(head));
                Some(FarAnswer {
                    at,
                    asked_us: request.at_us,
                    asked_by: request.src.as_str(),
                    bare,
                })
            })
            .collect();
        OnwardAnswers { answers, paired: BTreeSet::new() }
    }

    /// Whether the 2xx at `answer_at`, sent by an endpoint whose sockets are
    /// `endpoint` to an OPTIONS it took at `took_us`, carries back a bare far
    /// 2xx. It pairs with the first unpaired far 2xx it can carry back, bare
    /// or not, which pairs with no later answer.
    pub(super) fn relays_bare(
        &mut self,
        answer_at: usize,
        endpoint: [&str; 2],
        took_us: u64,
    ) -> bool {
        let far = self
            .answers
            .iter()
            .filter(|far| {
                far.at < answer_at
                    && took_us <= far.asked_us
                    && endpoint.contains(&far.asked_by)
                    && !self.paired.contains(&far.at)
            })
            .min_by_key(|far| far.at)
            .map(|far| (far.at, far.bare));
        let Some((at, bare)) = far else { return false };
        self.paired.insert(at);
        bare
    }
}
