use super::*;

pub(super) fn stack(leg: &str, nonce: &str) -> LegStack {
    let addr: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    LegStack::new(leg, addr, addr, nonce, "", "")
}

/// A leg's dialog carries the identity [`dialog_identity`] names for its
/// leg and nonce: what a lane computes before the run.
#[test]
fn a_leg_dials_under_the_identity_its_leg_and_nonce_name() {
    let identity = dialog_identity("A", "n1");
    assert_eq!(identity.call_id, "A-n1@pivot.invalid");
    assert_eq!(identity.local_tag, "A-n1-tag");
    let leg = stack("A", "n1");
    assert_eq!(leg.call_id(), identity.call_id);
    assert_eq!(leg.dialog.local_tag, identity.local_tag);
}

/// A UAS stack that has taken an inbound INVITE, and the INVITE itself.
pub(super) fn ringing(leg: &str) -> (LegStack, SipRequest) {
    let mut caller = stack("A", "r1a2b3");
    let addresses = Addresses {
        ruri: "sip:callee@pivot.invalid",
        from: "<sip:caller@pivot.invalid>",
        to: "<sip:callee@pivot.invalid>",
    };
    let invite = caller
        .out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, None)
        .expect("the caller composes its INVITE");
    let mut uas = stack(leg, "r1a2b3");
    uas.learn_request(&invite);
    (uas, invite)
}

pub(super) fn reliable(rseq: &str) -> Vec<TemplateHeader> {
    vec![TemplateHeader::frozen("Require", "100rel"), TemplateHeader::frozen("RSeq", rseq)]
}

/// A caller stack that has put its INVITE on the wire.
pub(super) fn calling() -> LegStack {
    calling_with_cseq(None)
}

pub(super) fn calling_with_cseq(cseq: Option<u32>) -> LegStack {
    let mut uac = stack("A", "r1a2b3");
    let addresses = Addresses {
        ruri: "sip:callee@pivot.invalid",
        from: "<sip:caller@pivot.invalid>",
        to: "<sip:callee@pivot.invalid>",
    };
    uac.out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, cseq)
        .expect("the caller composes its INVITE");
    uac
}

pub(super) fn to_tag(response: &SipResponse) -> String {
    response.to().tag().expect("a response under a dialog carries a To-tag").to_string()
}

pub(super) fn header(request: &SipRequest, name: sip_message::HeaderName) -> String {
    request.raw_text(name).next().map(|s| s.to_string()).unwrap_or_default()
}

/// RFC 3261 §12.1.2: a provisional carrying a new Contact refreshes the
/// early dialog's remote target. After `refresh_target` the leg's Contact
/// names a user part of its own, on that message and every later one.
#[test]
fn a_target_refresh_changes_the_contact_the_leg_states() {
    let (mut uas, _) = ringing("B");
    let ring = |uas: &mut LegStack| {
        let answer =
            Answer { status: 180, reason: "Ringing", cseq_method: Some("INVITE"), early_tag: None };
        let r = uas.respond(&answer, &[], Vec::new(), None).expect("a ring");
        r.contacts().as_slice().first().expect("a 180 states a Contact").uri().to_string()
    };
    let first = ring(&mut uas);
    uas.refresh_target(1);
    let second = ring(&mut uas);
    assert_ne!(first, second, "the refreshed target is another URI");
    assert!(second.contains("B-1@"), "{second}");
    assert_eq!(ring(&mut uas), second, "the refresh holds for later messages");
}

// ── UPDATE on an early dialog (RFC 3311 §5.1) ───────────────────────────

/// Compile once, run MANY: two instances of one plan must not share a
/// dialog identity, or the second run folds into the first's state.
#[test]
fn two_runs_of_one_leg_mint_disjoint_identities() {
    let first = stack("A", "r1a2b3");
    let second = stack("A", "r9z8y7");
    assert_ne!(first.call_id(), second.call_id());
    assert_ne!(first.local_tag(), second.local_tag());
    // The leg id stays in front, so a trace still reads.
    assert!(first.call_id().starts_with("A-"), "{}", first.call_id());
    assert!(first.local_tag().starts_with("A-"), "{}", first.local_tag());
}

#[test]
fn two_legs_of_one_run_mint_disjoint_identities() {
    let a = stack("A", "r1a2b3");
    let b = stack("B", "r1a2b3");
    assert_ne!(a.call_id(), b.call_id());
    assert_ne!(a.local_tag(), b.local_tag());
}

/// §8.1.1.7: a branch is unique across TIME, not merely within a run.
/// §17.2.3 matches a server transaction on branch + sent-by + method and
/// never on the Call-ID, so two runs on one socket would otherwise merge.
#[test]
fn two_runs_mint_disjoint_via_branches() {
    let mut first = stack("A", "r1a2b3");
    let mut second = stack("A", "r9z8y7");
    let branches = |s: &mut LegStack| {
        (0..3).map(|_| s.via().branch().unwrap_or_default().to_string()).collect::<Vec<_>>()
    };
    let one = branches(&mut first);
    let two = branches(&mut second);
    assert!(one.iter().all(|b| b.starts_with("z9hG4bK")), "{one:?}");
    for branch in &one {
        assert!(!two.contains(branch), "{branch} minted by both runs");
    }
    // And each run's own branches stay distinct per transaction.
    let mut sorted = one.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), one.len(), "{one:?}");
}
