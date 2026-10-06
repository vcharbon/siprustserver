//! The `Retry-After` (seconds) a refusal carries: never below [`MIN_SEC`],
//! and spread by a uniform jitter so a refused fleet does not retry in
//! lockstep.
//!
//! A value of `0` (RFC 3261 §20.33) asks for no wait, which a refusal must
//! never invite. Randomness is the caller's: [`jittered`] takes a roll, so
//! the same roll gives the same value.

/// The least `Retry-After` (seconds) a refusal carries.
pub const MIN_SEC: u32 = 1;

/// `hint_sec` floored at [`MIN_SEC`].
pub fn floored(hint_sec: u32) -> u32 {
    hint_sec.max(MIN_SEC)
}

/// Uniform over `[b, b + jitter_sec]` with `b = floored(base_sec)`, picked by
/// `roll` (any `u64`). `roll` is not consulted when `jitter_sec == 0`.
pub fn jittered(base_sec: u32, jitter_sec: u32, roll: impl FnOnce() -> u64) -> u32 {
    let base = floored(base_sec);
    if jitter_sec == 0 {
        return base;
    }
    // `jitter_sec + 1` fits in u64; the remainder is in `[0, jitter_sec]`.
    let offset = (roll() % (u64::from(jitter_sec) + 1)) as u32;
    base.saturating_add(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_is_floored_at_one_second() {
        assert_eq!(floored(0), 1);
        assert_eq!(floored(1), 1);
        assert_eq!(floored(30), 30);
    }

    #[test]
    fn zero_jitter_returns_the_base_without_rolling() {
        let mut rolled = false;
        let v = jittered(2, 0, || {
            rolled = true;
            999
        });
        assert_eq!(v, 2);
        assert!(!rolled, "zero jitter must not consult the roll");
    }

    #[test]
    fn the_roll_is_reduced_modulo_jitter_plus_one() {
        assert_eq!(jittered(10, 4, || 7), 12, "7 % 5 = 2");
        assert_eq!(jittered(10, 4, || 5), 10, "a multiple of jitter + 1 adds nothing");
        assert_eq!(jittered(10, 4, || 4), 14, "roll = jitter adds the most");
    }

    #[test]
    fn the_offset_stays_within_zero_through_jitter() {
        let (base, jitter) = (30u32, 6u32);
        for roll in 0u64..50 {
            let v = jittered(base, jitter, || roll);
            assert!((base..=base + jitter).contains(&v), "roll={roll} produced {v}");
        }
    }

    #[test]
    fn a_zero_base_never_hints_retry_now() {
        assert_eq!(jittered(0, 0, || 0), 1);
        for roll in 0u64..20 {
            let v = jittered(0, 4, || roll);
            assert!((1..=5).contains(&v), "roll={roll} produced {v}");
        }
    }

    #[test]
    fn extreme_inputs_do_not_overflow() {
        assert!((1..=10).contains(&jittered(1, 9, || u64::MAX)));
        assert_eq!(jittered(u32::MAX, u32::MAX, || u64::MAX), u32::MAX, "saturates");
    }
}
