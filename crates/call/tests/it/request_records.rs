//! The request records a call's dialogs hold (ADR-0055 §2): the record a
//! final answers, by its dialog and its client transaction.

use call::helpers::*;
use call::model::*;

use crate::model_helpers::{settled_call, B_LOCAL_TAG, B_TAG};

/// What a final to the UPDATE numbered `cseq` answers, on no branch.
fn update(cseq: i64) -> Answers<'static> {
    Answers { method: "UPDATE", cseq, branch: None }
}

/// A relay open in an early dialog the answer retired is found through that
/// dialog's tags, and closing it leaves the retired record in place (RFC 3261
/// §8.1.3.3).
#[test]
fn a_retired_dialogs_relay_is_found_by_its_tags_and_closed_in_place() {
    let mut call = settled_call();
    let open = RequestRecord {
        source_call_id: "cid".into(),
        source_from: "f".into(),
        source_to: "t".into(),
        offered: true,
        ..RequestRecord::relayed("UPDATE", 3, 3)
    };
    let lost = Dialog {
        requests: vec![open],
        ..Dialog::new(B_LOCAL_TAG, "lost-fork", DialogState::Retired, 3)
    };
    call.b_legs[0].dialogs.insert(lost);
    let (holder, pending) =
        answered_record(&call, "b-1", Some("lost-fork"), Some(B_LOCAL_TAG), update(3))
            .expect("found");
    let id = DialogId { our_tag: B_LOCAL_TAG.into(), peer_tag: "lost-fork".into() };
    assert_eq!(holder, RecordHolder::Retired(id.clone()));
    assert_eq!(pending.method, "UPDATE");
    assert!(answered_record(&call, "b-1", Some(B_TAG), Some(B_LOCAL_TAG), update(3)).is_none());

    let call = close_record(call, "b-1", &holder, 3);
    let kept = call.b_legs[0].dialogs.by_tags(&id.our_tag, &id.peer_tag).expect("kept");
    assert!(kept.is_retired() && kept.requests.is_empty());
}

/// A final settles the sent record its client transaction names: one to
/// another request reusing the record's CSeq — a dead owner's — answers
/// nothing here (RFC 3261 §17.1.3).
#[test]
fn a_final_answers_the_sent_record_of_its_branch_alone() {
    let mut call = settled_call();
    let sent = SentRequest {
        branch: "z9hG4bK-ours".into(),
        image: Vec::new(),
        destination: HostPort { host: "127.0.0.1".into(), port: 5070 },
    };
    let own = RequestRecord::own("NOTIFY", 5, sent, 0, Default::default());
    call.b_legs[0].dialogs.confirmed_mut().expect("confirmed").requests.push(own);
    let answers = |branch| Answers { method: "NOTIFY", cseq: 5, branch: Some(branch) };
    let to = Some(B_TAG);
    let from = Some(B_LOCAL_TAG);
    assert!(answered_record(&call, "b-1", to, from, answers("z9hG4bK-ours")).is_some());
    assert!(answered_record(&call, "b-1", to, from, answers("z9hG4bK-theirs")).is_none());
}
