use super::*;

/// Two batches never share an a-leg Call-ID, nor two cells of one batch.
#[test]
fn the_nonce_is_the_batch_seed_and_the_cell_ordinal() {
    assert_ne!(cell_nonce(0x10, 1), cell_nonce(0x10, 2));
    assert_ne!(cell_nonce(0x10, 0x11), cell_nonce(0x101, 1));
    assert_eq!(cell_nonce(0xab, 3), "00000000000000ab3");
}

/// One text, one seed; another text, another seed.
#[test]
fn the_seed_is_a_function_of_the_text() {
    assert_eq!(seed_of("alice-calls-bob"), seed_of("alice-calls-bob"));
    assert_ne!(seed_of("alice-calls-bob"), seed_of("bob-is-busy"));
    assert_eq!(seed_of("abc"), 0xa999_3e36_4706_816a, "sha1(\"abc\") starts a9993e36 4706816a");
}
