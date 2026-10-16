"""The readers on a hand-written run directory and a scripted VictoriaMetrics."""

from __future__ import annotations

import json
import urllib.parse
from collections.abc import Callable
from pathlib import Path

import pytest
from kit import T0, record, write_jsonl
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.executor import Call, FakeExecutor, HttpResult, ProcResult
from hakit.jsonval import Json
from hakit.readers.chart import ChartRequest, chart
from hakit.readers.drift import Drift, with_labels
from hakit.readers.drift_spec import DriftSpec, load_spec
from hakit.readers.logs import format_line, incarnations, query
from hakit.readers.obs_check import BASE_SCOPES, check, load_scopes, selectors
from hakit.readers.run_view import read_run
from hakit.readers.vmrange import Vm
from hakit.jsonl import read_jsonl
from hakit.verdict import compute

HOURS = 4
LOAD_S = HOURS * 3600
WINDOW_AT = 7200  # a kill two hours into the load
MIB = 2**20


def soak_dir(tmp_path: Path, with_verdict: bool) -> Path:
    """A 4 h run at 0.2 cps: shape `basic` sets up in 10 ms, shape `slow` slows by 30 ms/h."""
    recs: list[Json] = []
    for i in range(int(LOAD_S * 0.2)):
        t = T0 + i * 5000
        hours = i * 5 / 3600
        recs.append(record(f"b{i}", t, provisional=10))
        slow = record(f"s{i}", t + 1, provisional=10 + 30 * hours)
        slow["shape"] = "slow"
        recs.append(slow)
    profile: Json = {"duration_s": LOAD_S, "generator_stall_ms": 3_600_000}
    d = run_dir(tmp_path, recs, calls=len(recs), events=kill(1, T0 + WINDOW_AT * 1000, recovered_after=600_000), profile=profile)
    meta = json.loads((d / "run-meta.json").read_text())
    meta["ended_at_ms"] = T0 + (LOAD_S + 200) * 1000
    (d / "run-meta.json").write_text(json.dumps(meta))
    if with_verdict:
        (d / "verdict.json").write_text(json.dumps(compute(d, SCHEMAS)))
    return d


def vector(items: list[tuple[dict[str, str], float]]) -> str:
    result = [{"metric": m, "value": [0, str(v)]} for m, v in items]
    return json.dumps({"status": "success", "data": {"resultType": "vector", "result": result}})


def matrix(items: list[tuple[dict[str, str], Callable[[float], float]]], start: float, end: float, step: float) -> str:
    n = int((end - start) // step) + 1
    result = [{"metric": m, "values": [[start + i * step, str(f(start + i * step))] for i in range(n)]} for m, f in items]
    return json.dumps({"status": "success", "data": {"resultType": "matrix", "result": result}})


class Lab:
    """Worker RSS flat at 500 MiB but for a 900 MiB spike inside the window; worker
    CPU per call flat; the drained gauges and CDR counters as given."""

    def __init__(self, active_after_drain: float = 0, written: float = 95) -> None:
        self.active_after_drain = active_after_drain
        self.written = written
        self.queries: list[str] = []

    def rule(self, c: Call) -> HttpResult | None:
        if c.kind != "http":
            return None
        qs = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query)
        q = qs["query"][0]
        self.queries.append(q)
        if "/query_range" in c.url:
            start, end, step = (float(qs[k][0]) for k in ("start", "end", "step"))
            if "process_resident_memory_bytes" in q:

                def rss(t: float) -> float:
                    in_window = T0 / 1000 + WINDOW_AT <= t <= T0 / 1000 + WINDOW_AT + 300
                    return (900 if in_window else 500) * MIB

                return HttpResult(200, matrix([({"pod": "b2bua-worker-0"}, rss), ({"pod": "b2bua-worker-1"}, lambda _t: 400 * MIB)], start, end, step))
            if "container_cpu_usage_seconds_total" in q:
                return HttpResult(200, matrix([({}, lambda _t: 0.002)], start, end, step))
            return HttpResult(200, matrix([], start, end, step))
        if "b2bua_active_calls" in q:
            return HttpResult(200, vector([({}, self.active_after_drain)]))
        if q.startswith("sum(b2bua_"):
            return HttpResult(200, vector([({}, 0)]))
        if "b2bua_call_removals_by_class_total" in q:
            return HttpResult(200, vector([({"pod": "b2bua-worker-0"}, 100)]))
        if "b2bua_cdr_written_total" in q:
            return HttpResult(200, vector([({"pod": "b2bua-worker-0"}, self.written)]))
        if "b2bua_cdr_dropped_total" in q:
            return HttpResult(200, vector([({"pod": "b2bua-worker-0"}, 0)]))
        if "max(process_resident_memory_bytes" in q:
            return HttpResult(200, vector([({}, 520 * MIB)]))
        return HttpResult(200, vector([]))


def drift(tmp_path: Path, lab: Lab, with_verdict: bool = True) -> Json:
    d = soak_dir(tmp_path, with_verdict)
    vm = Vm(FakeExecutor([lab.rule]), "vm")
    return Drift(read_run(d), vm, DriftSpec(), d / "drift").run()


def checks(doc: Json) -> dict[str, Json]:
    return {jv.string(c, "name"): c for c in jv.objs(doc, "checks")}


def test_drift_leaves_out_the_warm_up_and_the_verdict_s_windows(tmp_path: Path) -> None:
    doc = drift(tmp_path, Lab())
    c = checks(doc)
    rss = c["worker RSS b2bua-worker-0"]
    # The spike inside the window is left out: a flat 500 MiB passes.
    assert rss["verdict"] == "pass"
    fit_doc = jv.as_obj(rss["fit"])
    assert (fit_doc["first"], fit_doc["last"]) == (500.0, 500.0)
    # Grid samples every 60 s from the warm-up end (+1800 s) to the load end
    # (+14400 s), less those in the window [+7200 s, +7200 + 600 + 40 s].
    assert fit_doc["samples"] == 211 - 11
    assert doc["excluded_windows"] == [[T0 / 1000 + WINDOW_AT, T0 / 1000 + WINDOW_AT + 640]]
    assert c["worker RSS after drain b2bua-worker-0"]["verdict"] == "pass"


def test_a_rate_sample_off_the_median_offered_load_is_left_out(tmp_path: Path) -> None:
    base = checks(drift(tmp_path, Lab()))["worker CPU per call"]
    (tmp_path / "run").rename(tmp_path / "first")
    d = soak_dir(tmp_path, True)
    # Half the calls of the fourth hour go: its rate windows are off the median.
    recs = [r for r in read_jsonl(d / "records.jsonl") if not (T0 + 3 * 3_600_000 <= jv.number(r, "started_at_ms") < T0 + 4 * 3_600_000 and str(r["call_id"]).startswith("s"))]
    write_jsonl(d / "records.jsonl", recs)
    thin = Drift(read_run(d), Vm(FakeExecutor([Lab().rule]), "vm"), DriftSpec(), d / "drift").run()
    got = checks(thin)["worker CPU per call"]
    assert jv.number(jv.as_obj(got["fit"]), "samples") < jv.number(jv.as_obj(base["fit"]), "samples") - 50


def test_without_a_verdict_the_windows_come_from_the_events(tmp_path: Path) -> None:
    doc = drift(tmp_path, Lab(), with_verdict=False)
    assert checks(doc)["worker RSS b2bua-worker-0"]["verdict"] == "pass"


def test_setup_latency_per_shape_comes_from_the_records(tmp_path: Path) -> None:
    c = checks(drift(tmp_path, Lab()))
    assert c["setup latency mean basic"]["verdict"] == "pass"
    assert c["setup latency p99 basic"]["verdict"] == "pass"
    assert c["setup latency mean slow"]["verdict"] == "fail"
    assert "late slope +30.0 ms/h" in jv.string(c["setup latency mean slow"], "detail")


def test_gauges_held_after_the_drain_and_unaccounted_cdrs_fail(tmp_path: Path) -> None:
    c = checks(drift(tmp_path, Lab()))
    assert c["gauges after drain"]["verdict"] == "pass"
    assert (c["CDR per call b2bua-worker-0"]["verdict"], c["CDR per call b2bua-worker-0"]["detail"]) == (
        "pass",
        "+5 completed calls with no CDR outcome: 100 completed - 95 written - 0 dropped (bound 10)",
    )
    (tmp_path / "run").rename(tmp_path / "first")
    doc = drift(tmp_path, Lab(active_after_drain=3, written=80))
    c = checks(doc)
    assert c["gauges after drain"]["verdict"] == "fail"
    assert c["CDR per call b2bua-worker-0"]["verdict"] == "fail"
    assert doc["verdict"] == "fail"


def test_a_signal_without_series_is_na(tmp_path: Path) -> None:
    doc = drift(tmp_path, Lab())
    c = checks(doc)
    assert c["call-limiter RSS"]["verdict"] == "na"
    assert c["worker jemalloc allocated b2bua-worker-0"]["verdict"] == "na"
    assert doc["verdict"] == "fail"  # the slow shape; an na ranks below a fail
    assert (tmp_path / "run" / "drift" / "drift.json").is_file()
    assert (tmp_path / "run" / "drift" / "process_resident_memory_bytes.svg").read_text().startswith("<svg")


def test_selectors_take_extra_labels() -> None:
    assert with_labels('m{class="t"}', 'namespace="n"') == 'm{namespace="n",class="t"}'
    assert with_labels("m", 'namespace="n"') == 'm{namespace="n"}'


def test_a_spec_overrides_only_what_it_names(tmp_path: Path) -> None:
    p = tmp_path / "spec.yaml"
    p.write_text("namespace: prod\nbounds:\n  memory: [2, 4]\ninstance_containers: [db]\n")
    s = load_spec(p)
    assert (s.namespace, s.bounds.memory, s.instance_containers, s.worker_pods) == ("prod", (2.0, 4.0), ["db"], "b2bua-worker-.*")


def test_a_run_chart_shades_its_windows_and_marks_its_phases(tmp_path: Path) -> None:
    d = soak_dir(tmp_path, True)
    vm = Vm(FakeExecutor([Lab().rule]), "vm")
    svg = chart(vm, ChartRequest("RSS", ["max(process_resident_memory_bytes)"], ["rss"], markers=("bad", f"{T0 / 1000 + 10}:mine")), read_run(d))
    assert svg.count('fill="#e0653f" fill-opacity=".12"') == 1
    assert "kill_worker inject" in svg and "kill_worker healed" in svg and ">mine<" in svg
    assert ">rss:b2bua-worker-0<" in svg
    with pytest.raises(ValueError):
        chart(vm, ChartRequest("x", [], []))


def test_logs_name_each_container_incarnation(tmp_path: Path) -> None:
    rows = [
        {"_time": "t1", "kubernetes.pod_name": "w-0", "kubernetes.container_name": "w", "file": "/x/w-0_ns_w-" + "a" * 64 + ".log", "_msg": "one"},
        {"_time": "t2", "kubernetes.pod_name": "w-0", "kubernetes.container_name": "w", "file": "/x/w-0_ns_w-" + "b" * 64 + ".log", "_msg": "two"},
        {"_time": "t3", "kubernetes.pod_name": "w-0", "kubernetes.container_name": "w", "file": "/x/w-0_ns_w-" + "b" * 64 + ".log", "_msg": "three"},
    ]
    fake = FakeExecutor([lambda c: HttpResult(200, "".join(json.dumps(r) + "\n" for r in rows))])
    got = query(fake, "vl", "ns", 1, 2, "w-.*")
    assert format_line(got[0]) == f"t1 w-0 w c={'a' * 12} uid=? | one"
    assert incarnations(got) == [f"w-0 w c={'a' * 12} uid=? lines=1 first=t1 last=t1", f"w-0 w c={'b' * 12} uid=? lines=2 first=t2 last=t3"]
    assert 'kubernetes.namespace_name%3A%3D%22ns%22' in fake.calls[0].url
    with pytest.raises(ValueError):
        query(FakeExecutor([lambda c: HttpResult(500, "down")]), "vl", "ns", 1, 2, "w")


def test_obs_check_scopes_include_and_name_the_namespace(tmp_path: Path) -> None:
    p = tmp_path / "spec.yaml"
    p.write_text('scopes:\n  lane:\n    include: [sut]\n    series: ["my_calls{namespace=\\"${namespace}\\"}", "kube_pod_info"]\n')
    scopes = load_scopes(p)
    got = selectors(scopes, ["lane"], "lab")
    assert got[:5] == BASE_SCOPES["cluster"].series
    assert got[-1] == 'my_calls{namespace="lab"}' and got.count("kube_pod_info") == 1
    assert 'b2bua_active_calls{namespace="lab"}' in got
    p.write_text("scopes:\n  sut: {series: [x]}\n")
    with pytest.raises(ValueError, match="the base's"):
        load_scopes(p)
    p.write_text("scopes:\n  a: {include: [b]}\n  b: {include: [a]}\n")
    with pytest.raises(ValueError, match="includes itself"):
        selectors(load_scopes(p), ["a"], "n")
    with pytest.raises(ValueError, match="unknown scope"):
        selectors(BASE_SCOPES, ["nope"], "n")


def test_obs_check_runs_the_base_check_script() -> None:
    fake = FakeExecutor([lambda c: ProcResult(1, "", "missing: x\n")])
    res = check(fake, "http://vm", ["x", "y"], 30)
    (call,) = fake.calls
    assert call.argv[0].endswith("deploy/observability/check.sh") and call.argv[1:] == ["--wait", "30", "x", "y"]
    assert call.env == {"VM": "http://vm"} and res.rc == 1


def test_drift_s_setup_latency_leaves_out_measurements_across_a_generator_stall(tmp_path: Path) -> None:
    from hakit.readers.drift import latency_points

    d = soak_dir(tmp_path, True)
    profile = json.loads((d / "profile.json").read_text())
    profile["generator_stall_ms"] = 500
    profile["streams"] = [{**s, "cps": 0.2} for s in profile["streams"]]
    (d / "profile.json").write_text(json.dumps(profile))
    recs = read_jsonl(d / "records.jsonl")
    # A gap of 10 s where a start is due every 5 s: a stall (1/cps 5 s + 0.5 s).
    gap_from, gap_to = T0 + 7_000_000, T0 + 7_010_000
    recs = [r for r in recs if not gap_from < jv.number(r, "started_at_ms") < gap_to]
    late = record("late", gap_from, provisional=9_000)
    recs.append(late)
    write_jsonl(d / "records.jsonl", recs)
    points = latency_points(read_run(d))
    assert all(ms < 9_000 for pts in points.values() for _, ms in pts)
    assert len(points["basic"]) + len(points["slow"]) == len(recs) - 1  # `late` alone


def test_drift_exits_non_zero_on_a_failing_or_unjudged_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from hakit.cli import main

    monkeypatch.setenv("HAKIT_VM_URL", "http://127.0.0.1:9")
    d = soak_dir(tmp_path, True)
    # The slow shape fails from the records; the series, unread, are na.
    assert main(["drift", str(d)]) == 1


class ProbedLab(Lab):
    """A front proxy's health probes hold 128 transactions with no call; the
    run's last teardowns hold `at_end` more when the run ends, `settled` more
    `settle` seconds later."""

    def __init__(self, at_end: float, settled: float, settle: float) -> None:
        super().__init__()
        self.at_end, self.settled, self.settle = at_end, settled, settle

    def rule(self, c: Call) -> HttpResult | None:
        if c.kind == "http" and "b2bua_txn_active_transactions" in c.url and "/query_range" not in c.url:
            qs = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query)
            self.queries.append(qs["query"][0])
            t = float(qs["time"][0])
            ended = (T0 + (LOAD_S + 200) * 1000) / 1000
            extra = 0.0 if t < ended - 3600 else (self.at_end if t < ended + self.settle else self.settled)
            return HttpResult(200, vector([({}, 128 + extra)]))
        return super().rule(c)


def test_a_baseline_gauge_must_return_to_its_value_before_the_load(tmp_path: Path) -> None:
    spec = DriftSpec(
        drained_zero=["b2bua_active_calls"],
        drained_baseline=["b2bua_txn_active_transactions"],
        drained_baseline_tolerance=4,
        drained_settle_s=47,
    )

    def baseline(sub: str, lab: ProbedLab) -> Json:
        d = soak_dir(tmp_path / sub, True)
        return checks(Drift(read_run(d), Vm(FakeExecutor([lab.rule]), "vm"), spec, d / "drift").run())["gauges back to their baseline"]

    # 41 more at the run's end, back to the probes' 128 once the timers ran.
    settled = ProbedLab(41, 0, 47)
    assert baseline("settled", settled)["verdict"] == "pass"
    # The baseline is the median of the span before the load, not one scrape.
    assert any(q.startswith("quantile_over_time(0.5, sum(b2bua_txn_active_transactions") for q in settled.queries)
    # Still 20 above it after the settle: a leak.
    held = baseline("held", ProbedLab(41, 20, 47))
    assert held["verdict"] == "fail" and "148 47 s after the run vs 128 before the load" in jv.string(held, "detail")
