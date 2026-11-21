//! The tags an entry records: its message's From tag and To tag. A response
//! copies its request's From and To (RFC 3261 §8.2.6.2), so a request either
//! side of a dialog sends and its answer carry the same pair, the sender's
//! tag in From.

use super::*;

/// `(method, code, from_tag, to_tag)` of an entry.
type TagRow<'a> = (&'a str, Option<u16>, Option<&'a str>, Option<&'a str>);

/// The [`TagRow`] of every entry.
fn tags(entries: &[MessageEntry]) -> Vec<TagRow<'_>> {
    entries
        .iter()
        .map(|e| (e.method.as_str(), e.code, e.from_tag.as_deref(), e.to_tag.as_deref()))
        .collect()
}

/// The callee hangs up: his BYE carries his tag in From and this stack's in
/// To, and the BYE this stack sends the caller its own a-leg tag in From and
/// hers in To.
#[tokio::test(start_paused = true)]
async fn each_entry_records_the_tag_of_the_side_that_sent_its_request() {
    let h = Harness::new("message-ring-tags");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut bob_bye = bob_dialog.bye().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob_bye.expect(200).await;

    let done = sut.assert_reaped().await;
    let (alice_tag, a_tag) = (Some(alice_dialog.local_tag()), Some(alice_dialog.remote_tag()));
    let (bob_tag, b_tag) = (Some(bob_dialog.local_tag()), Some(bob_dialog.remote_tag()));
    assert_eq!(
        tags(&done.a_leg.messages.entries),
        vec![
            ("INVITE", None, alice_tag, None),
            ("INVITE", Some(100), alice_tag, None),
            ("INVITE", Some(200), alice_tag, a_tag),
            ("ACK", None, alice_tag, a_tag),
            ("BYE", None, a_tag, alice_tag),
            ("BYE", Some(200), a_tag, alice_tag),
        ],
        "the a-leg: alice's requests and their answers carry her tag in From, this stack's BYE its own"
    );
    assert_eq!(
        tags(&b_leg(&done).messages.entries),
        vec![
            ("INVITE", None, b_tag, None),
            ("INVITE", Some(200), b_tag, bob_tag),
            ("ACK", None, b_tag, bob_tag),
            ("BYE", None, bob_tag, b_tag),
            ("BYE", Some(200), bob_tag, b_tag),
        ],
        "the b-leg: this stack's requests carry its tag in From, bob's BYE and its answer his"
    );

    let _report = h.finish().await;
}
