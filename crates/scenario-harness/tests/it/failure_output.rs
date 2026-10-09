//! A failing run says why in its first line: a step failure names the party,
//! what it expected and got, the step's ordinal and the test line; a timeout
//! names the awaited message and its dialog; an audit or dialog-table failure
//! says it is one, with the rule and the party. The failure dump that follows
//! opens with the same digest and keeps the wire ladder and the UAs' tables.

use std::rc::Rc;

use scenario_harness::{
    expect_failure, Agent, CseqOp, CseqOpAt, CseqPattern, Dialog, DumpSection, FailureView, Harness,
};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

/// alice calls bob; returns her dialog and bob's.
async fn established(alice: &Agent, bob: &Agent) -> (Dialog, Dialog) {
    let mut call = alice.invite(bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    (dialog, uas.dialog())
}

/// The dump's header line carries the digest.
fn assert_dump_opens_with(dump: &str, name: &str, digest: &str) {
    let header = dump.trim_start().lines().next().unwrap_or_default();
    assert_eq!(header, format!("══ FAILED '{name}': {digest} ══"), "{dump}");
}

/// A wrong status names the party, both statuses, the step's ordinal and the
/// test line that awaited it; the dump keeps the ladder and the UAs' tables.
#[tokio::test(start_paused = true)]
async fn a_wrong_status_names_the_party_the_step_and_the_test_line() {
    let line = std::cell::Cell::new(0);
    let failed = expect_failure(async {
        let h = Harness::new("digest-wrong-status");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
        bob.receive("INVITE").await.respond(486, "Busy Here").await;
        line.set(line!() + 1);
        call.expect(200).await;
    })
    .await;
    let at = format!("{}:{}", file!(), line.get());
    assert_eq!(
        failed.digest(),
        format!("alice expected 200, got 486 Busy Here at step 2 (expect at {at})")
    );
    let [dump] = failed.dumps.as_slice() else { panic!("one dump: {:?}", failed.dumps) };
    assert_dump_opens_with(dump, "digest-wrong-status", failed.digest());
    assert!(dump.contains("── wire ladder ──"), "{dump}");
    assert!(dump.contains("486 Busy Here"), "the ladder shows the answer: {dump}");
    assert!(dump.contains("── UA dialog tables ──"), "{dump}");
    assert!(dump.contains("ended by a 486 final"), "bob's table shows the ended dialog: {dump}");
}

/// A server-side receive that times out names the method it awaited and the
/// dialogs the UA holds, the ones the request should name.
#[tokio::test(start_paused = true)]
async fn a_receive_timeout_names_the_awaited_request_and_the_dialogs_held() {
    let failed = expect_failure(async {
        let h = Harness::new("digest-receive-timeout");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let (_alice_dialog, _bob_dialog) = established(&alice, &bob).await;
        bob.receive("BYE").await;
    })
    .await;
    let digest = failed.digest();
    assert!(
        digest.starts_with(
            "bob timed out after 5s waiting for a BYE request in one of its dialogs: Call-ID "
        ),
        "{digest}"
    );
    assert!(
        digest.contains(" local bob-tag-") && digest.contains(" remote alice-tag-"),
        "{digest}"
    );
    assert!(digest.contains("(confirmed) at step 4 (receive at "), "{digest}");
}

/// A client-side expect that times out names the status, the request it
/// answers and the dialog's Call-ID and tags.
#[tokio::test(start_paused = true)]
async fn an_expect_timeout_names_the_awaited_response_and_its_dialog() {
    let failed = expect_failure(async {
        let h = Harness::new("digest-expect-timeout");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let (mut alice_dialog, _bob_dialog) = established(&alice, &bob).await;
        let mut info = alice_dialog.send_request(InDialogMethod::Info).send().await;
        bob.receive("INFO").await;
        info.expect(200).await;
    })
    .await;
    let digest = failed.digest();
    assert!(
        digest.starts_with("alice timed out after 5s waiting for a 200 to INFO CSeq 2 (Call-ID "),
        "{digest}"
    );
    assert!(
        digest.contains("From-tag alice-tag-") && digest.contains("To-tag bob-tag-"),
        "{digest}"
    );
}

/// A run red on the RFC audit says so apart from an assertion failure: the
/// rule, the party it charges and the offending message, with the ladder.
#[tokio::test(start_paused = true)]
async fn an_audit_failure_names_the_rule_the_party_and_the_message() {
    let failed = expect_failure(async {
        let h = Harness::new("digest-audit");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let (mut dialog, _bob_dialog) = established(&alice, &bob).await;
        // alice reuses her CSeq on a second INFO (RFC 3261 §12.2.1.1).
        dialog.set_cseq_pattern(CseqPattern {
            offset: 0,
            ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }],
        });
        for _ in 0..2 {
            let mut info = dialog.send_request(InDialogMethod::Info).send().await;
            bob.receive("INFO").await.respond(200, "OK").await;
            info.expect(200).await;
        }
        let mut bye = dialog.bye().await;
        bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
        h.finish().await;
    })
    .await;
    let digest = failed.digest();
    assert!(
        digest.starts_with(
            "red because RFC audit rule cseq-in-dialog-order fired (charged to alice) at message #"
        ),
        "{digest}"
    );
    assert!(digest.contains("alice → bob  INFO"), "the offending message: {digest}");
    let [dump] = failed.dumps.as_slice() else { panic!("one dump: {:?}", failed.dumps) };
    assert_dump_opens_with(dump, "digest-audit", digest);
    assert!(dump.contains("── wire ladder ──") && dump.contains("BYE"), "{dump}");
}

/// A run red on a UA's dialog table says so, naming the rule, the UA and the
/// request, and the dump shows the verdict under that UA's table.
#[tokio::test(start_paused = true)]
async fn a_dialog_table_failure_names_the_rule_and_the_ua() {
    let failed = expect_failure(async {
        let h = Harness::new("digest-dialog");
        h.allow_violation("in-dialog-from-tag", "bob forges a From-tag on purpose");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let (mut alice_dialog, mut bob_dialog) = established(&alice, &bob).await;
        // bob's OPTIONS under a From-tag alice never saw.
        let (mut txn, sent) = bob_dialog
            .send_request(InDialogMethod::Options)
            .try_send_with_request()
            .await
            .expect("the OPTIONS leaves");
        alice.receive("OPTIONS").await.respond(200, "OK").await;
        txn.expect(200).await;
        let text = String::from_utf8(sent.image().to_vec()).expect("text");
        let tag = sent.from().tag().expect("bob's tag").to_string();
        let wire = text.replacen(&format!("tag={tag}"), "tag=bob-forged", 1).replacen(
            "branch=z9hG4bK",
            "branch=z9hG4bK-forged",
            1,
        );
        bob.try_send_datagram(wire.as_bytes(), alice.addr()).await.expect("sent");
        h.advance(std::time::Duration::from_millis(200)).await;
        alice.drain_tu().await;
        let mut bye = alice_dialog.bye().await;
        bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
        h.finish().await;
    })
    .await;
    let digest = failed.digest();
    assert!(
        digest.starts_with(
            "red because dialog rule dialog-unknown fired on alice: OPTIONS From-tag bob-forged \
             names no dialog"
        ),
        "{digest}"
    );
    let [dump] = failed.dumps.as_slice() else { panic!("one dump: {:?}", failed.dumps) };
    assert!(dump.contains("── wire ladder ──"), "{dump}");
    assert!(
        dump.contains("  alice:\n") && dump.contains("verdict [dialog-unknown]: OPTIONS"),
        "{dump}"
    );
}

/// A SUT whose task met a failure the scenario could not see, and the
/// section it adds to the dump.
struct BrokenSut;

impl FailureView for BrokenSut {
    fn cause(&self) -> Option<String> {
        Some("model invariant one-dialog violated after a turn: two dialogs\nCall { .. }".into())
    }

    fn sections(&self) -> Vec<DumpSection> {
        vec![DumpSection { title: "sut ledger".into(), lines: vec!["timer T1".into()] }]
    }
}

/// A failure a SUT met out of the scenario's sight leads the dump's digest,
/// the scenario's own failure following it; the SUT's section is in the dump.
#[tokio::test(start_paused = true)]
async fn a_suts_hidden_failure_leads_the_digest() {
    let failed = expect_failure(async {
        let h = Harness::new("digest-hidden-cause");
        h.add_failure_view(Rc::new(BrokenSut));
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        bob.receive("INVITE").await;
    })
    .await;
    let [dump] = failed.dumps.as_slice() else { panic!("one dump: {:?}", failed.dumps) };
    let mut lines = dump.trim_start().lines();
    assert_eq!(
        lines.next(),
        Some(
            "══ FAILED 'digest-hidden-cause': red because model invariant one-dialog violated \
             after a turn: two dialogs ══"
        ),
        "{dump}"
    );
    let then = lines.next().unwrap_or_default();
    assert!(then.starts_with("   then: bob timed out after 5s waiting for a INVITE"), "{dump}");
    assert!(dump.contains("── sut ledger ──\n  timer T1\n"), "{dump}");
}

/// `finish` fails on a SUT's hidden failure, its message first.
#[tokio::test(start_paused = true)]
async fn finish_fails_on_a_suts_hidden_failure() {
    let failed = expect_failure(async {
        let h = Harness::new("finish-hidden-cause");
        h.add_failure_view(Rc::new(BrokenSut));
        h.finish().await;
    })
    .await;
    assert_eq!(failed.digest(), "model invariant one-dialog violated after a turn: two dialogs");
    let [dump] = failed.dumps.as_slice() else { panic!("one dump: {:?}", failed.dumps) };
    assert!(dump.contains("── wire ladder ──"), "{dump}");
}
