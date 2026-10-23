"""The capacity ramp's step judgement, its limits and the CPU fit."""

from __future__ import annotations

import pytest

from hakit.jsonval import Json
from hakit.readers.ramp import completed_cps, cpu_fit, judge, limits, shed_seen_cps


def step(label: str, accepted: float, rejected: float, cores: float, **over: object) -> Json:
    """A clean step: the stepped streams completed every accepted call and saw every refusal as a 503."""
    r: Json = {
        "label": label,
        "stepped": {
            "cps": {"completed": accepted, "shed_503": rejected, "other_fail": 0},
            "n": {"completed": accepted * 60, "shed_503": rejected * 60, "other_fail": 0},
        },
        "setup_p99_ms": 100,
        "worker_accepted_cps": accepted,
        "worker_rejected_cps": rejected,
        "worker_cpu_cores": cores,
    }
    r.update(over)
    return r


def test_the_callers_counts_add_every_stream() -> None:
    r = step("a", 100, 10, 1, background={"completed_cps": 5, "shed_503_cps": 1, "failed": 0, "shed_503": 0})
    assert completed_cps(r) == 105 and shed_seen_cps(r) == 11
    assert completed_cps({"label": "a"}) is None


def test_a_clean_step_and_each_way_to_disturb_it() -> None:
    assert judge(step("a", 100, 0, 1), 200) == (True, "")
    assert judge(step("a", 100, 0, 1, setup_p99_ms=500), 200) == (False, "setup p99 500 ms")
    bg = {"completed_cps": 0, "shed_503_cps": 0, "failed": 3, "shed_503": 1}
    assert judge(step("a", 100, 0, 1, background=bg), 200) == (False, "background failures 2")
    # 5 cps apart is past 2 % of 105 plus 2 cps; 4 is within 2 % of 104 plus 2.
    split = {"cps": {"completed": 100, "shed_503": 0}, "n": {"other_fail": 0}}
    assert judge(step("a", 105, 0, 1, stepped=split), 200) == (
        False,
        "worker accepted 105.0 cps vs callers 100.0 (tolerance 4.1)",
    )
    assert judge(step("a", 104, 0, 1, stepped=split), 200)[0] is True
    failing = {"cps": {"completed": 100, "shed_503": 0}, "n": {"other_fail": 2}}
    assert judge(step("a", 100, 0, 1, stepped=failing), 200) == (False, "stepped failures 2")
    assert judge({"label": "a"}, 200) == (None, "no stepped stream records")
    assert judge({**step("a", 1, 0, 1), "worker_rejected_cps": None}, 200) == (None, "no new-call counts")
    assert judge({**step("a", 1, 0, 1), "setup_p99_ms": None}, 200) == (None, "no setup p99")


def test_the_limits_are_the_highest_clean_steps() -> None:
    failing = {"cps": {"completed": 300, "shed_503": 200}, "n": {"other_fail": 1}}
    rows = [step("s1", 100, 0, 1), step("s2", 200, 50, 2.2), step("s3", 300, 200, 3, stepped=failing)]
    lim = limits(rows, 200)
    assert lim["accepted_call_capacity"] == {"cps": 200, "step": "s2"}
    assert lim["reject_throughput"] == {"cps": 50, "step": "s2"}
    assert lim["steps"] == {
        "s1": {"clean": True, "why": ""},
        "s2": {"clean": True, "why": ""},
        "s3": {"clean": False, "why": "stepped failures 1"},
    }


def test_the_cpu_fit_separates_accepted_and_rejected_costs() -> None:
    # cores = 0.01 * accepted + 0.002 * rejected
    rows = [step("a", 100, 0, 1.0), step("b", 100, 100, 1.2), step("c", 200, 50, 2.1)]
    f = cpu_fit(rows, {"a": True, "b": True, "c": True})
    assert f is not None
    assert f["ms_per_accepted_call"] == pytest.approx(10) and f["ms_per_rejected_call"] == pytest.approx(2)
    assert f["r2"] == pytest.approx(1) and f["flags"] == []
    assert cpu_fit(rows, {"a": True}) is None


def test_the_cpu_fit_says_when_it_cannot_separate_the_costs() -> None:
    no_reject = cpu_fit([step("a", 100, 0, 1), step("b", 200, 0, 2)], {"a": True, "b": True})
    assert no_reject is not None and no_reject["note"] == "no fitted step rejected" and no_reject["ms_per_rejected_call"] is None
    collinear = cpu_fit([step("a", 100, 10, 1), step("b", 200, 20, 2)], {"a": True, "b": True})
    assert collinear is not None and collinear["ms_per_accepted_call"] is None and str(collinear["note"]).startswith("accepted and rejected are collinear")
    saturated = [step("a", 100, 0, 1), step("b", 200, 0, 1.9, worker_cpu_limit_cores=2), step("c", 50, 0, 0.5, worker_throttled_s=1)]
    f = cpu_fit(saturated, {"a": True, "b": True, "c": True})
    assert f is None  # one step left after the saturated and the throttled ones
