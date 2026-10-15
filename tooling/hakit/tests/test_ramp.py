"""The capacity ramp's step judgement, its limits and the CPU fit."""

from __future__ import annotations

import pytest

from hakit.jsonval import Json
from hakit.readers.ramp import completed_cps, cpu_fit, judge, limits, p99_ms, shed_seen_cps


def step(label: str, accepted: float, rejected: float, cores: float, **over: object) -> Json:
    """A clean step: SIPp completed every accepted call and saw every refusal as a 503."""
    r: Json = {
        "label": label,
        "sipp_split": {
            "cps": {"completed": accepted, "shed_503": rejected},
            "n": {"other_fail": 0, "dead_call": 0, "out_of_call": 0},
        },
        "sipp_setup_ms_q": {"0.99": "<100"},
        "worker_accepted_cps": accepted,
        "worker_rejected_cps": rejected,
        "worker_cpu_cores": cores,
    }
    r.update(over)
    return r


def test_p99_reads_the_repartition_bound() -> None:
    assert p99_ms(step("a", 1, 0, 1)) == 100
    assert p99_ms({**step("a", 1, 0, 1), "sipp_setup_ms_q": {"0.99": ">=1000"}}) == float("inf")
    assert p99_ms({"label": "a"}) is None


def test_the_callers_counts_add_every_source() -> None:
    r = step("a", 100, 10, 1, soak_sipp={"completed_cps": 5, "shed_503_cps": 1}, loadgen_ok_cps=3, loadgen_503_cps=2, loadgen_refused_overload_cps=1)
    assert completed_cps(r) == 108 and shed_seen_cps(r) == 14
    assert shed_seen_cps({**r, "soak_sipp": {"completed_cps": 5}}) is None


def test_a_clean_step_and_each_way_to_disturb_it() -> None:
    assert judge(step("a", 100, 0, 1), 200) == (True, "")
    assert judge(step("a", 100, 0, 1, sipp_setup_ms_q={"0.99": "<500"}), 200) == (False, "setup p99 <500 ms")
    assert judge(step("a", 100, 0, 1, loadgen_not_ok={"timeout": 3}), 200) == (False, "loadgen timeout 3")
    # 5 cps apart is past 2 % of 105 plus 2 cps; 4 is within 2 % of 104 plus 2.
    assert judge(step("a", 105, 0, 1, sipp_split={"cps": {"completed": 100, "shed_503": 0}, "n": {"other_fail": 0}}), 200) == (
        False,
        "worker accepted 105.0 cps vs callers 100.0 (tolerance 4.1)",
    )
    assert judge(step("a", 104, 0, 1, sipp_split={"cps": {"completed": 100, "shed_503": 0}, "n": {"other_fail": 0}}), 200)[0] is True
    disturbed = step("a", 100, 0, 1, sipp_split={"cps": {"completed": 100, "shed_503": 0}, "n": {"other_fail": 2, "dead_call": 1}})
    assert judge(disturbed, 200) == (False, "SIPp other failures 2; SIPp dead-call/out-of-call messages 1")
    assert judge({"label": "a"}, 200) == (None, "no SIPp split")
    assert judge({**step("a", 1, 0, 1), "worker_rejected_cps": None}, 200) == (None, "no new-call counts")


def test_the_limits_are_the_highest_clean_steps() -> None:
    rows = [step("s1", 100, 0, 1), step("s2", 200, 50, 2.2), step("s3", 300, 200, 3, loadgen_not_ok={"status_500": 1})]
    lim = limits(rows, 200)
    assert lim["accepted_call_capacity"] == {"cps": 200, "step": "s2"}
    assert lim["reject_throughput"] == {"cps": 50, "step": "s2"}
    assert lim["steps"] == {"s1": {"clean": True, "why": ""}, "s2": {"clean": True, "why": ""}, "s3": {"clean": False, "why": "loadgen status_500 1"}}


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
