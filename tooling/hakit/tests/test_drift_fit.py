"""The drift judgement: fits, relative and absolute bounds, per-instance checks."""

from __future__ import annotations

import pytest

from hakit.readers.drift_fit import fit, judge, judge_abs, judge_instance, judge_instances, run_verdict, worst


def line(slope_per_h: float, base: float = 100, hours: float = 4, step_s: float = 60) -> list[tuple[float, float]]:
    n = int(hours * 3600 / step_s)
    return [(i * step_s, base + slope_per_h * i * step_s / 3600) for i in range(n + 1)]


def test_a_straight_line_fits_its_slope_and_spans() -> None:
    f = fit(line(10))
    assert f is not None
    assert f.slope_per_h == pytest.approx(10)
    assert f.late_slope_per_h == pytest.approx(10)
    assert f.slope_pct_per_h == pytest.approx(10 * 100 / f.first)
    assert f.span_s == 3600 and f.clean_s == 4 * 3600
    assert f.first == pytest.approx(105, abs=0.2) and f.last == pytest.approx(135, abs=0.2)
    assert fit([(0, 1), (1, 2), (2, 3)]) is None


def test_a_high_water_mark_flattens_the_late_slope() -> None:
    pts = [(t, min(v, 120)) for t, v in line(20)]
    f = fit(pts)
    assert f is not None and f.late_slope_per_h == pytest.approx(0) and f.slope_per_h > 0


def test_relative_judgement_fails_only_on_a_late_slope_and_growth() -> None:
    assert judge(fit(line(10)), 5, 10)[0] == "fail"  # ~9 %/h late, +28 %
    assert judge(fit([(t, min(v, 140)) for t, v in line(30)]), 5, 10)[0] == "warn"  # grown, flat late
    assert judge(fit(line(3)), 5, 10)[0] == "warn"  # late slope over half its bound
    assert judge(fit(line(0.5)), 5, 10)[0] == "pass"
    assert judge(None, 5, 10) == ("na", "too few clean samples")


def test_absolute_judgement_reads_units() -> None:
    v, d = judge_abs(fit(line(15, base=50)), 10, 20)
    assert v == "fail" and d.startswith("first ") and "late slope +15.0 ms/h (bound 10)" in d
    assert judge_abs(fit(line(4, base=50)), 10, 20)[0] == "pass"


def test_an_instance_is_judged_only_on_a_long_late_span() -> None:
    short = fit(line(5, hours=1))
    assert judge_instance(short, 10, 2, 2700, "MiB")[0] == "na"
    assert judge_instance(fit(line(1, hours=2)), 10, 2, 2700)[0] == "pass"  # within the absolute bound
    # A small process: 1.5 MiB/h on 10 MiB is 15 %/h, yet within 2 MiB/h.
    assert judge_instance(fit(line(1.5, base=10, hours=2)), 10, 2, 2700)[0] == "pass"
    assert judge_instance(fit(line(20, hours=2)), 10, 2, 2700)[0] == "fail"  # 20 MiB/h on 100: 18 %/h late
    assert judge_instance(fit(line(7, hours=2)), 10, 2, 2700)[0] == "warn"


def test_instances_count_only_when_one_was_judged() -> None:
    none = judge_instances([("a", fit(line(5, hours=1)))], 10, 2, 2700)
    assert (none.verdict, none.counts) == ("na", False)
    some = judge_instances([("a", fit(line(5, hours=1))), ("b", fit(line(20, hours=2))), ("c", fit(line(7, hours=2)))], 10, 2, 2700)
    assert (some.verdict, some.counts) == ("fail", True)
    assert some.detail.startswith("worst judged instance b:") and some.detail.endswith("2 of 3 instances judged")


def test_the_run_verdict_is_the_worst_counted_check() -> None:
    assert worst(["pass", "warn"]) == "warn" and worst(["warn", "na"]) == "na" and worst(["na", "fail"]) == "fail"
    assert run_verdict([("pass", True), ("fail", False), ("warn", True)]) == "warn"
    assert run_verdict([("fail", False)]) == "na"
