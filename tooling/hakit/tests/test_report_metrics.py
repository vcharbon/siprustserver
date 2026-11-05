"""The series a profile asks the report to read per window: kept in the run
directory, shown in the verdict and the summary, a reading that changes nothing."""

from __future__ import annotations

import json
from pathlib import Path

from kit import T0, record
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.executor import FakeExecutor, HttpResult
from hakit.profile import ReportMetric, parse_profile
from hakit.report_metrics import collect, query
from hakit.summary import render
from hakit.verdict import compute
from hakit.vm import VictoriaMetrics
from hakit.windows import Window, build_windows

AT = T0 + 100_000
COUNTER = ReportMetric("sut_dropped_total", "increase")
GAUGE = ReportMetric("sut_stalled", "max")


def test_a_counter_reads_its_increase_and_a_gauge_its_maximum_per_pod() -> None:
    assert query(COUNTER, 90) == "sum by (pod) (increase(sut_dropped_total[90s]))"
    assert query(GAUGE, 90) == "max by (pod) (max_over_time(sut_stalled[90s]))"


def test_the_read_lands_in_the_verdict_and_the_summary_and_changes_no_status(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [record("ok", AT + 1_000)], calls=1, events=kill(1, AT))
    kept = {"001-kill_worker": [{"metric": "sut_dropped_total", "read": "increase", "pods": {"w-1": 12.0, "w-0": 0.0}}]}
    (d / "report-metrics.json").write_text(json.dumps(kept))
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    assert v["status"] == "pass"
    (w,) = jv.objs(v, "windows")
    assert jv.objs(w, "metrics") == kept["001-kill_worker"]
    assert "- read `sut_dropped_total` (increase): w-0 0, w-1 12" in render(v)


def test_a_run_whose_profile_names_none_has_no_row(tmp_path: Path) -> None:
    v = compute(run_dir(tmp_path, [record("ok", AT + 1_000)], calls=1, events=kill(1, AT)), SCHEMAS)
    (w,) = jv.objs(v, "windows")
    assert "metrics" not in w


def collected(tmp_path: Path, answer: HttpResult) -> tuple[Path, dict[str, object], list[str]]:
    urls: list[str] = []

    def rule(call: object) -> HttpResult | None:
        url = getattr(call, "url", "")
        if "/api/v1/query" in url:
            urls.append(url)
            return answer
        return None

    d = run_dir(tmp_path, [record("ok", AT + 1_000)], calls=1, events=kill(1, AT), profile={"generator_stall_ms": 3_600_000})
    raw = json.loads((d / "profile.json").read_text())
    raw["report_metrics"] = [{"metric": "sut_dropped_total", "read": "increase"}]
    windows: list[Window] = build_windows([json.loads(ln) for ln in (d / "events.jsonl").read_text().splitlines()])
    collect(parse_profile(raw), windows, VictoriaMetrics(FakeExecutor([rule]), "http://vm"), d)
    return d, json.loads((d / "report-metrics.json").read_text()), urls


def test_collect_reads_each_window_and_keeps_the_value_per_pod(tmp_path: Path) -> None:
    body = '{"data":{"result":[{"metric":{"pod":"w-1"},"value":[0,"7"]}]}}'
    _, doc, urls = collected(tmp_path, HttpResult(200, body))
    assert doc == {"001-kill_worker": [{"metric": "sut_dropped_total", "read": "increase", "pods": {"w-1": 7.0}}]}
    assert len(urls) == 1 and "time=" in urls[0]


def test_a_failed_read_is_kept_as_its_error_and_the_verdict_stands(tmp_path: Path) -> None:
    d, doc, _ = collected(tmp_path, HttpResult(503, "no"))
    (row,) = doc["001-kill_worker"]  # type: ignore[misc]
    assert "HTTP 503" in row["error"]  # type: ignore[index]
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    assert "- read `sut_dropped_total` (increase): VictoriaMetrics did not answer" in render(v)
