//! [`TurnIds`]: the identifiers one turn draws, a function of the key, the
//! turn and the draw's place in it.

use crate::key::IdKey;
use crate::render;

/// The draws of one turn: draw `n` is HMAC(key, `turn_seq` ‖ `n`), so a turn
/// replayed with its sequence number draws the same identifiers, and two
/// turns under one key share none. The caller allocates `turn_seq` once per
/// turn and never reuses it under a key. Not `Clone` or `Copy`: a copy would
/// draw the same identifiers twice.
#[derive(Debug)]
pub struct TurnIds {
    key: IdKey,
    turn_seq: u64,
    n: u64,
}

impl TurnIds {
    /// The draws of turn `turn_seq` under `key`, none drawn yet.
    pub fn new(key: &IdKey, turn_seq: u64) -> Self {
        Self { key: key.clone(), turn_seq, n: 0 }
    }

    /// The draws of turn `turn_seq` under the all-zero key every test shares.
    #[cfg(feature = "testkit")]
    pub fn fixed(turn_seq: u64) -> Self {
        Self::new(&IdKey::new(&[0u8; 32]), turn_seq)
    }

    /// The turn these draws belong to.
    pub fn turn_seq(&self) -> u64 {
        self.turn_seq
    }

    /// The next uniform 64-bit draw, for a value picked at random (a
    /// `Retry-After`).
    pub fn draw(&mut self) -> u64 {
        let out = self.key.draw(&[&self.turn_seq.to_le_bytes(), &self.n.to_le_bytes()]);
        self.n = self.n.wrapping_add(1);
        out
    }

    /// The next From/To tag.
    pub fn new_tag(&mut self) -> String {
        render::tag(self.draw())
    }

    /// The next Via branch.
    pub fn new_branch(&mut self) -> String {
        render::branch(self.draw())
    }

    /// The next CSeq or RSeq start.
    pub fn new_sequence_number(&mut self) -> u32 {
        render::sequence_number(self.draw())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn key(seed: u8) -> IdKey {
        IdKey::new(&[seed; 32])
    }

    #[test]
    fn a_turn_replayed_draws_the_same_identifiers() {
        let (mut a, mut b) = (TurnIds::new(&key(1), 7), TurnIds::new(&key(1), 7));
        for _ in 0..4 {
            assert_eq!(a.new_branch(), b.new_branch());
        }
        assert_eq!(a.new_tag(), b.new_tag());
    }

    /// Two turns under one key, and a turn and the key's counter stream,
    /// share no draw.
    #[test]
    fn turns_and_the_counter_stream_share_no_draw() {
        let k = key(2);
        let mut seen: HashSet<u64> = (0..1_000).map(|n| k.counter_draw(n)).collect();
        for turn_seq in 0..100 {
            let mut turn = TurnIds::new(&k, turn_seq);
            for _ in 0..10 {
                assert!(seen.insert(turn.draw()), "turn {turn_seq} repeats a draw");
            }
        }
    }

    #[test]
    fn two_keys_draw_unrelated_turns() {
        let (mut a, mut b) = (TurnIds::new(&key(3), 0), TurnIds::new(&key(4), 0));
        let drawn: HashSet<u64> = (0..1_000).map(|_| a.draw()).collect();
        assert!((0..1_000).all(|_| !drawn.contains(&b.draw())));
    }

    #[test]
    fn a_turn_names_its_sequence_number() {
        assert_eq!(TurnIds::new(&key(5), 42).turn_seq(), 42);
    }

    #[test]
    fn a_fixed_turn_is_the_zero_key_s() {
        let mut fixed = TurnIds::fixed(9);
        let mut keyed = TurnIds::new(&IdKey::new(&[0u8; 32]), 9);
        assert_eq!(fixed.new_tag(), keyed.new_tag());
    }
}
