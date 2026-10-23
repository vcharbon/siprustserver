//! How one OPTIONS answer is read, and the probe's per-branch call ref.

use super::*;

/// ADR-0050 §3: the status sets health, `X-Readiness` acceptance.
#[test]
fn an_answer_reads_on_two_axes() {
    let read = |status, r: Option<Readiness>| read_answer(status, r.as_ref());
    let alive = Some(WorkerHealth::Alive);
    let (accept, refuse) = (Some(Acceptance::Accept), Some(Acceptance::Refuse));
    assert_eq!(
        read(200, Some(Readiness::accept())),
        AnswerReading { health: alive, acceptance: accept }
    );
    assert_eq!(
        read(200, Some(Readiness::refuse("backend"))),
        AnswerReading { health: alive, acceptance: refuse }
    );
    assert_eq!(
        read(200, None),
        AnswerReading { health: alive, acceptance: accept },
        "absent reads accept"
    );
    assert_eq!(
        read(503, None),
        AnswerReading { health: None, acceptance: refuse },
        "health as it was"
    );
    assert_eq!(
        read(503, Some(Readiness::accept())),
        AnswerReading { health: None, acceptance: refuse },
        "a 503 refuses whatever it states"
    );
    for status in [404, 486, 500, 603] {
        assert_eq!(
            read(status, None),
            AnswerReading { health: alive, acceptance: None },
            "{status}"
        );
    }
}

#[test]
fn probe_call_ref_is_branch_scoped() {
    assert_eq!(probe_call_ref("z9hG4bK-1"), "probe:z9hG4bK-1");
    assert_ne!(probe_call_ref("a"), probe_call_ref("b"));
}
