use pivot_schema::bundle::VerdictStatus;

use super::*;

fn green() -> RunVerdict {
    RunVerdict::ok("c", "simulated")
}

#[test]
fn a_reaped_run_leaves_the_verdict_green() {
    let mut verdict = green();
    record(&[Ok(()), Ok(())], &mut verdict);
    assert_eq!(verdict.status, VerdictStatus::Ok);
    assert!(verdict.failures.is_empty(), "{:?}", verdict.failures);
}

#[test]
fn a_breach_fails_the_verdict_at_the_cleanup_site() {
    let mut verdict = green();
    record(&[Ok(()), Err("lock leak: 1 stranded lock".into())], &mut verdict);
    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(
        matches!(&verdict.failures[..], [Failure::CheckFailed { site, observed, .. }]
            if site == "post-run cleanup" && observed == "lock leak: 1 stranded lock"),
        "{:?}",
        verdict.failures
    );
    assert!(charges(&verdict.failures[0]));
}

#[test]
fn a_breach_both_stages_saw_is_stated_once_and_distinct_ones_all() {
    let leak = "call leak: creations (2) != removals (1)".to_string();
    let mut verdict = green();
    record(&[Err(leak.clone()), Err(leak.clone())], &mut verdict);
    assert!(
        matches!(&verdict.failures[..], [Failure::CheckFailed { observed, .. }]
            if *observed == leak),
        "{:?}",
        verdict.failures
    );
    let mut verdict = green();
    record(&[Err(leak.clone()), Err("mark leak".into())], &mut verdict);
    assert!(
        matches!(&verdict.failures[..], [Failure::CheckFailed { observed, .. }]
            if *observed == format!("{leak}; mark leak")),
        "{:?}",
        verdict.failures
    );
}
