"""Recovery maths (ADR-0049 §3.6): baseline, the first span back, ratios not counts."""

from __future__ import annotations

from kit import T0

from hakit.model import RecoveryRule
from hakit.recovery import Baseline, Sample, StreamSamples, is_back, measure, p99, recover

RULE = RecoveryRule(tolerance=0.1, baseline_s=60, hold_s=10, latency_slack_ms=50)


def paced(from_ms: float, to_ms: float, cps: float, ok: bool = True, latency: float | None = 10) -> list[Sample]:
    step = 1000 / cps
    n = int((to_ms - from_ms) / step)
    return [Sample(from_ms + i * step, ok, latency) for i in range(n)]


def test_p99_is_the_nearest_rank() -> None:
    assert p99([]) is None
    assert p99([5.0]) == 5.0
    values = [float(i) for i in range(1, 101)]
    assert p99(values) == 99.0
    assert p99([*values, 1000.0]) == 100.0  # 101 values: rank ceil(99.99) = 100


def test_the_baseline_is_the_ratio_and_p99_of_the_calls_started_before_inject() -> None:
    samples = paced(T0, T0 + 60_000, 10) + [Sample(T0 + 59_999, False, None)]
    base = measure(StreamSamples(samples).span(T0, T0 + 60_000))
    assert base == Baseline(600 / 601, 10)
    assert measure([]) is None


def test_back_takes_the_ratio_tolerance_below_and_the_latency_slack_above() -> None:
    base = Baseline(1.0, 100)
    assert is_back(Baseline(0.9, 160), base, RULE)  # 0.9 = 1.0 x 0.9; 160 = 100 x 1.1 + 50
    assert not is_back(Baseline(0.8999, 100), base, RULE)
    assert not is_back(Baseline(1.0, 160.01), base, RULE)
    assert not is_back(Baseline(1.0, None), base, RULE)
    assert is_back(Baseline(1.0, None), Baseline(1.0, None), RULE)


def test_a_stream_is_back_at_the_start_of_the_first_span_within_tolerance() -> None:
    inject, healed = T0 + 60_000, T0 + 61_000
    samples = paced(T0, inject, 10)
    samples += paced(inject, T0 + 75_000, 10, ok=False, latency=None)  # nothing sets up until +75 s
    samples += paced(T0 + 75_000, T0 + 200_000, 10)
    r = recover("s", StreamSamples(samples), inject, healed, T0 + 300_000, RULE)
    assert r is not None and r.baseline == Baseline(1.0, 10)
    # The first 1 s step whose 10 s span has at most 10 % failed: [74 s, 84 s) holds 10 of 100.
    assert r.back_at_ms == T0 + 74_000 and r.recovery_s(healed) == 13.0


def test_slow_setups_keep_a_stream_from_being_back() -> None:
    inject, healed = T0 + 60_000, T0 + 60_000
    samples = paced(T0, inject, 10) + paced(inject, T0 + 90_000, 10, latency=500) + paced(T0 + 90_000, T0 + 200_000, 10)
    r = recover("s", StreamSamples(samples), inject, healed, T0 + 300_000, RULE)
    assert r is not None and r.recovery_s(healed) == 30.0


def test_a_rate_change_is_no_impact() -> None:
    inject = T0 + 60_000
    samples = paced(T0, inject, 20) + paced(inject, T0 + 200_000, 2)
    r = recover("s", StreamSamples(samples), inject, inject, T0 + 300_000, RULE)
    assert r is not None and r.recovery_s(inject) == 0.0


def test_no_call_before_inject_means_no_baseline_and_no_span_means_not_back() -> None:
    inject = T0 + 60_000
    assert recover("s", StreamSamples(paced(inject, T0 + 200_000, 10)), inject, inject, T0 + 300_000, RULE) is None
    quiet = StreamSamples(paced(T0, inject, 10))
    r = recover("s", quiet, inject, inject, T0 + 120_000, RULE)
    assert r is not None and r.back_at_ms is None and r.recovery_s(inject) is None


def test_no_span_starting_at_or_after_the_limit_counts() -> None:
    inject = T0 + 60_000
    samples = paced(T0, inject, 10) + paced(inject, T0 + 100_000, 10, ok=False) + paced(T0 + 100_000, T0 + 200_000, 10)
    # [99 s, 109 s) is the first span back: 10 failures of 100.
    early = recover("s", StreamSamples(samples), inject, inject, T0 + 99_000, RULE)
    assert early is not None and early.back_at_ms is None
    r = recover("s", StreamSamples(samples), inject, inject, T0 + 99_001, RULE)
    assert r is not None and r.back_at_ms == T0 + 99_000
