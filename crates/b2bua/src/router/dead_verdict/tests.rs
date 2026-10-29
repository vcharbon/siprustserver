//! A notice is taken only signed under the cluster key and fresh, and each
//! verdict once.

use sip_message::dead_notice::DeadNotice;

use super::*;

/// The default freshness window and hold.
const FRESH_MS: i64 = crate::repl::DEFAULT_VERDICT_FRESH_MS;
const HELD_MS: i64 = crate::repl::DEFAULT_VERDICT_HOLD_MS;

fn key(byte: u8) -> SigningKey {
    SigningKey::new(&[byte; 32]).expect("a 32-byte key")
}

fn signed(by: &SigningKey, at_ms: i64) -> ReceivedNotice {
    let notice = DeadNotice { ordinal: "w0".into(), at_ms };
    let fields = notice.signed_fields();
    let signature = by.sign(&fields.each_ref().map(Vec::as_slice)).to_vec();
    ReceivedNotice { notice, signature: Some(signature) }
}

/// `verify` under the default freshness window.
fn check(key: Option<&SigningKey>, received: &ReceivedNotice, now_ms: i64) -> Result<(), Refusal> {
    verify(key, received, now_ms, FRESH_MS)
}

#[test]
fn a_notice_signed_under_the_cluster_key_and_fresh_is_taken() {
    let now = 100_000;
    assert_eq!(check(Some(&key(1)), &signed(&key(1), now), now), Ok(()));
    assert_eq!(check(Some(&key(1)), &signed(&key(1), now - FRESH_MS), now), Ok(()));
    assert_eq!(check(Some(&key(1)), &signed(&key(1), now + FRESH_MS), now), Ok(()));
}

#[test]
fn an_unsigned_badly_signed_or_stale_notice_is_refused() {
    let now = 100_000;
    let mut unsigned = signed(&key(1), now);
    unsigned.signature = None;
    assert_eq!(check(Some(&key(1)), &unsigned, now), Err(Refusal::Unsigned));
    assert_eq!(check(Some(&key(1)), &signed(&key(2), now), now), Err(Refusal::BadSignature));
    let mut forged = signed(&key(1), now);
    forged.notice.ordinal = "w1".into();
    assert_eq!(check(Some(&key(1)), &forged, now), Err(Refusal::BadSignature));
    let old = signed(&key(1), now - FRESH_MS - 1);
    assert_eq!(check(Some(&key(1)), &old, now), Err(Refusal::Stale));
    let ahead = signed(&key(1), now + FRESH_MS + 1);
    assert_eq!(check(Some(&key(1)), &ahead, now), Err(Refusal::Stale));
}

#[test]
fn a_node_with_no_cluster_key_trusts_no_notice() {
    let now = 100_000;
    assert_eq!(check(None, &signed(&key(1), now), now), Err(Refusal::NoKey));
}

#[test]
fn each_verdict_is_taken_once_and_a_later_one_again() {
    let book = VerdictBook::default();
    assert!(!book.holds("w0", 1_000, 1_000, HELD_MS));
    assert!(book.take("w0", 1_000), "the first copy");
    assert!(book.holds("w0", 1_000, 1_500, HELD_MS), "a repeat of the same verdict is held");
    assert!(!book.take("w0", 1_000), "and is no new verdict");
    assert!(!book.holds("w1", 1_000, 1_500, HELD_MS), "another worker's verdict is not held");
    assert!(
        !book.holds("w0", 4_000, 4_000, HELD_MS),
        "a later verdict on the same worker is not held"
    );
}

#[test]
fn a_verdict_is_held_through_its_notices_last_repeat() {
    let book = VerdictBook::default();
    assert!(book.take("w0", 1_000));
    assert!(
        book.holds("w0", 1_000, 1_000 + FRESH_MS + 1, HELD_MS),
        "a repeat past freshness is held"
    );
    assert!(book.holds("w0", 1_000, 1_000 + HELD_MS, HELD_MS));
    assert!(!book.holds("w0", 1_000, 1_000 + HELD_MS + 1, HELD_MS), "dropped past the hold");
    assert!(locked(&book.taken, DEAD_VERDICTS).is_empty());
}
