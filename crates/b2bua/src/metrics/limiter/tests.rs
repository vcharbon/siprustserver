use super::*;

/// Every label value is rendered, zero included, and a count lands on
/// its own series only.
#[test]
fn every_series_is_rendered_with_its_closed_labels() {
    let m = LimiterCounters::default();
    m.count_failure(LimiterOp::Admit, LimiterFailure::BreakerOpen);
    m.count_failure(LimiterOp::Health, LimiterFailure::BadAnswer);
    m.count_refresh_answer(RefreshOutcome::Dropped);
    m.count_release_given_up(ReleaseGiveUp::Shutdown, 3);
    m.count_task_restart(LimiterTask::BreakerProbe);
    m.set_uncounted_calls(2);
    let mut s = String::new();
    m.render(&mut s);
    for line in [
        "b2bua_limiter_failures_total{op=\"admit\",cause=\"breaker_open\"} 1",
        "b2bua_limiter_failures_total{op=\"admit\",cause=\"timeout\"} 0",
        "b2bua_limiter_failures_total{op=\"health\",cause=\"bad_answer\"} 1",
        "b2bua_limiter_failures_total{op=\"release\",cause=\"status\"} 0",
        "b2bua_limiter_requests_total{op=\"release\"} 0",
        "b2bua_limiter_refresh_answers_total{outcome=\"dropped\"} 1",
        "b2bua_limiter_refresh_answers_total{outcome=\"extended\"} 0",
        "b2bua_limiter_release_given_up_total{reason=\"shutdown\"} 3",
        "b2bua_limiter_task_restarts_total{task=\"breaker_probe\"} 1",
        "b2bua_limiter_uncounted_calls 2",
        "b2bua_limiter_release_flushes_total{outcome=\"given_up\"} 0",
        "# TYPE b2bua_limiter_uncounted_calls gauge",
    ] {
        assert!(s.contains(&format!("{line}\n")), "{line} missing in\n{s}");
    }
    assert!(!s.contains("op=\"release\",cause=\"bad_answer\""), "a release body is never read");
    assert!(!s.contains("op=\"refresh\",cause=\"breaker_open\""), "only an admit is not sent");
    assert_eq!(m.failures_of(LimiterOp::Admit), 1);
}
