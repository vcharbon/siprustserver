//! ADR-0024 §6 load-lane waiver unification, end-to-end over a real sampled
//! `AgentBinder` recording: a CSeq reuse (a §12.2.1.1 violation) is recorded on
//! the isolated fake fabric, then filtered through the structural `WaiverScope`
//! path (`AgentBinder::rfc_findings`). Proves (1) a rule-only WaiverScope filters
//! byte-for-byte like the historic `HashSet<String>` allow-list, and (2) the
//! "audit-green is not proof" control — with the waiver REMOVED the finding
//! FIRES, for both a rule-only and a party-scoped waiver.

use std::time::Duration;

use scenario_harness::{
    AgentBinder, CseqOp, CseqOpAt, CseqPattern, WaiverScope, ANSWER_SDP, OFFER_SDP,
};
use sip_message::generators::InDialogMethod;

const RULE: &str = "rfc3261.cseqInDialogOrder";

/// Record a SUT-less two-party call on the fake fabric in which alice replays a
/// captured CSeq REUSE on her first in-dialog request (OPTIONS reuses the
/// INVITE's CSeq — the §12.2.1.1 violation the audit flags), then hangs up.
async fn record_reuse_call() -> AgentBinder {
    let binder = AgentBinder::fake(1, Duration::from_secs(5), true);
    let alice = binder.agent("alice", "127.0.0.1:5060").await;
    let bob = binder.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Reuse at step 0: the OPTIONS reuses the INVITE's CSeq (1) — not greater
    // than the prior in-dialog number, so the audit fires.
    dialog.set_cseq_pattern(CseqPattern {
        offset: 0,
        ops: vec![CseqOpAt { at: 0, op: CseqOp::Reuse }],
    });
    let mut opt = dialog.send_request(InDialogMethod::Options).send().await;
    let mut obob = bob.receive("OPTIONS").await;
    obob.respond(200, "OK").await;
    opt.expect(200).await;

    let mut bye = dialog.bye().await;
    let mut bbob = bob.receive("BYE").await;
    bbob.respond(200, "OK").await;
    bye.expect(200).await;

    binder
}

fn has_rule(findings: &[sip_net::RfcFinding], rule: &str) -> bool {
    findings.iter().any(|f| f.rule == rule)
}

/// Test 1: a rule-only WaiverScope (what a case's `allowViolations` lowers to)
/// filters the audit byte-for-byte like the historic `HashSet<String>` allow-list.
#[tokio::test]
async fn rule_only_waiver_matches_the_historic_hashset_filter() {
    let binder = record_reuse_call().await;

    let all = binder.rfc_findings(&[]).findings;
    assert!(has_rule(&all, RULE), "the reuse fires the CSeq audit (sanity)");

    // The OLD behaviour: drop every finding whose rule is in the allow set.
    let old_filtered: Vec<sip_net::RfcFinding> =
        all.iter().filter(|f| f.rule != RULE).cloned().collect();

    // The NEW behaviour: a rule-only, conditional WaiverScope (the exact lowering
    // of a case's `allowViolations`).
    let new_filtered = binder
        .rfc_findings(&[WaiverScope::rule(RULE, "case allowViolations").conditional()])
        .findings;

    assert_eq!(new_filtered, old_filtered, "rule-only WaiverScope filters byte-for-byte");
}

/// Test 2: "audit-green is not proof". With the waiver REMOVED the finding FIRES;
/// with it present the finding is filtered — for BOTH a rule-only and a
/// party-scoped waiver.
#[tokio::test]
async fn audit_green_is_not_proof_rule_and_party_scoped() {
    let binder = record_reuse_call().await;

    // Control: no waiver ⇒ the finding is PRESENT (the audit fires).
    assert!(
        has_rule(&binder.rfc_findings(&[]).findings, RULE),
        "with no waiver the audit fires — green requires a waiver, not clean code",
    );

    // A RULE-only waiver removes it, and is marked used.
    let ruled = binder.rfc_findings(&[WaiverScope::rule(RULE, "coarse")]);
    assert!(!has_rule(&ruled.findings, RULE), "rule-only waiver filters the finding");
    assert_eq!(ruled.used, vec![true], "the covering waiver is marked used");

    // A PARTY-scoped waiver on alice (who emitted the reuse) removes it too.
    let partied = binder.rfc_findings(&[WaiverScope::rule(RULE, "alice").on_party("alice")]);
    assert!(!has_rule(&partied.findings, RULE), "party-scoped waiver on alice covers it");
    assert_eq!(partied.used, vec![true], "the party-scoped waiver is marked used");

    // A party-scoped waiver on the WRONG party does NOT cover — and stays unused.
    let wrong = binder.rfc_findings(&[WaiverScope::rule(RULE, "bob").on_party("bob")]);
    assert!(has_rule(&wrong.findings, RULE), "a waiver on the wrong party leaves it gated");
    assert_eq!(wrong.used, vec![false], "the non-covering waiver stays unused");

    // Removing the waiver again ⇒ the finding FIRES ⇒ audit-green was only the
    // waiver's doing, never a clean trace.
    assert!(has_rule(&binder.rfc_findings(&[]).findings, RULE), "removing the waiver re-fires it");
}
