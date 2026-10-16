"""The host under the run (ADR-0049 §3.12, §6.5): pressure and swap sampled
every second into `host.jsonl` and pushed as series, a run refused while the
host is under stress, and a stall that coincides with stress reported as
`host_under_stress`, first, with its numbers. `/proc` is a fake reader."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from kit import T0, paced, record
from rundir import SCHEMAS, run_dir

from hakit import jsonval as jv
from hakit.clock import FakeClock
from hakit.executor import Call, FakeExecutor
from hakit.host import HOST_FILE, HostSampler, Thresholds, intervals, preflight_errors, read_sample
from hakit.jsonl import read_jsonl, write_jsonl
from hakit.jsonval import Json
from hakit.summary import render
from hakit.verdict import compute
from hakit.vm import VictoriaMetrics


class FakeProc:
    """`/proc/pressure/*` and `/proc/vmstat` from counters a test moves."""

    def __init__(self) -> None:
        self.total: dict[tuple[str, str], int] = {(r, k): 0 for r in ("cpu", "memory", "io") for k in ("some", "full")}
        self.avg10: dict[tuple[str, str], float] = {key: 0.0 for key in self.total}
        self.pswpin = 0
        self.pswpout = 0

    def rule(self, c: Call) -> str | None:
        if c.kind != "read" or c.path is None:
            return None
        if c.path.parent == Path("/proc/pressure"):
            r = c.path.name
            return "".join(
                f"{k} avg10={self.avg10[(r, k)]:.2f} avg60=0.00 avg300=0.00 total={self.total[(r, k)]}\n"
                for k in ("some", "full")
            )
        if c.path == Path("/proc/vmstat"):
            return f"nr_free_pages 1\npswpin {self.pswpin}\npswpout {self.pswpout}\n"
        return None


def test_an_interval_s_share_stalled_comes_from_the_totals() -> None:
    proc = FakeProc()
    fake = FakeExecutor([proc.rule])
    a = read_sample(fake, T0)
    proc.total[("io", "full")] += 120_000  # 120 ms stalled over 2 s: 6 %
    proc.pswpin += 300
    b = read_sample(fake, T0 + 2000)
    assert a is not None and b is not None
    (iv,) = intervals([a, b])
    assert round(iv.pct[("io", "full")], 3) == 6.0 and iv.swapin_pages_s == 150
    assert iv.stressed(Thresholds()) and not iv.stressed(Thresholds(io_full_pct=7, swapin_pages_s=151))


def test_a_host_without_pressure_files_is_never_sampled_again() -> None:
    clock = FakeClock()
    fake = FakeExecutor([])
    sampler = HostSampler("r", Path("/nonexistent"), fake, clock, VictoriaMetrics(fake, "vm"))
    sampler.tick()
    clock.sleep(5)
    sampler.tick()
    assert sampler.next_ms() is None
    assert len([c for c in fake.calls if c.kind == "read" and "pressure" in str(c.path)]) == 1


def test_the_sampler_keeps_each_second_and_pushes_the_interval_it_closes(tmp_path: Path) -> None:
    proc = FakeProc()
    clock = FakeClock()
    fake = FakeExecutor([proc.rule])
    sampler = HostSampler("r", tmp_path, fake, clock, VictoriaMetrics(fake, "http://vm"))
    sampler.tick()
    sampler.tick()  # not due yet
    clock.sleep(1)
    proc.total[("memory", "full")] += 300_000
    sampler.tick()
    assert len(read_jsonl(tmp_path / HOST_FILE)) == 2 and sampler.next_ms() == clock.now_ms() + 1000
    (push,) = [c for c in fake.calls if c.kind == "http"]
    body = (push.body or b"").decode()
    assert 'hakit_host_pressure_ratio{run="r",resource="memory",kind="full"} 0.3 ' in body
    assert 'hakit_host_swap_pages_per_second{run="r",direction="in"} 0 ' in body


def test_by_default_swap_in_is_stress_and_a_passing_io_stall_is_not() -> None:
    """Swap-in means a generator's own pages may be on disk; a 10 % io full
    share is the host's disk writeback, which a tmpfs-backed generator does not
    wait on; 20 % is a sustained stall."""
    proc = FakeProc()
    fake = FakeExecutor([proc.rule])
    a = read_sample(fake, T0)
    proc.total[("io", "full")] += 100_000  # 10 % of 1 s
    b = read_sample(fake, T0 + 1000)
    proc.total[("io", "full")] += 200_000  # 20 %
    c = read_sample(fake, T0 + 2000)
    proc.pswpin += 50
    d = read_sample(fake, T0 + 3000)
    assert a is not None and b is not None and c is not None and d is not None
    calm, io_stall, swapping = intervals([a, b, c, d])
    assert not calm.stressed(Thresholds())
    assert io_stall.stressed(Thresholds()) and swapping.stressed(Thresholds())


def test_preflight_refuses_a_host_under_stress_and_names_what() -> None:
    proc = FakeProc()
    proc.avg10[("io", "full")] = 7.5
    fake = FakeExecutor([proc.rule])
    assert preflight_errors(fake, FakeClock(), Thresholds()) == []
    proc.avg10[("io", "full")] = 22.5
    errs = preflight_errors(fake, FakeClock(), Thresholds())
    assert errs == ["io full pressure avg10 22.5 % is at or above 20 %"]
    proc.avg10[("io", "full")] = 0.5
    assert preflight_errors(fake, FakeClock(), Thresholds()) == []
    # No PSI: nothing to refuse.
    assert preflight_errors(FakeExecutor([]), FakeClock(), Thresholds()) == []


def host_lines(stressed_from: float, stressed_until: float, io_full_us: int = 200_000, swapin: int = 400) -> list[Json]:
    """One sample a second over the first two minutes; the seconds from
    `stressed_from` to `stressed_until` stall io `io_full_us` and swap in `swapin` pages each."""
    lines: list[Json] = []
    total = pages = 0
    for i in range(121):
        at = T0 + i * 1000
        if stressed_from < at <= stressed_until:
            total += io_full_us
            pages += swapin
        psi = {k: {"avg10": 0.0, "total": 0} for k in ("some", "full")}
        lines.append({"at_ms": at, "cpu": psi, "memory": psi, "io": {"some": {"avg10": 0.0, "total": total},
                      "full": {"avg10": 0.0, "total": total}}, "pswpin": pages, "pswpout": 0})
    return lines


def stalled_records() -> list[Json]:
    """20 cps for 120 s, no start for a second from T0 + 60 s, and the call
    started into it read its provisional late: one spoiled measurement."""
    at = T0 + 60_000
    recs = [r for r in paced("p", T0, T0 + 120_000, 20) if not at < r["started_at_ms"] < at + 1000]  # type: ignore[operator]
    return [record("late", at, provisional=1005, setup=5000, answered=5000) if r["started_at_ms"] == at else r for r in recs]


def verdict(tmp_path: Path, host: list[Json], profile: Json | None = None) -> Json:
    recs = stalled_records()
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile=profile or {})
    write_jsonl(d / HOST_FILE, host)
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    return v


def test_a_stall_inside_host_stress_is_host_under_stress_first_with_its_numbers(tmp_path: Path) -> None:
    v = verdict(tmp_path, host_lines(T0 + 58_000, T0 + 63_000))
    kinds = [i["kind"] for i in jv.objs(v, "integrity")]
    assert kinds == ["host_under_stress", "generator_stall"] and v["status"] == "invalid"
    h = jv.objs(v, "integrity")[0]
    assert (h["at_ms"], h["until_ms"]) == (T0 + 58_000, T0 + 63_000)
    assert h["host"] == {"io_full_pct": 20.0, "memory_full_pct": 0.0, "swapin_pages_s": 400.0}
    assert str(h["detail"]).startswith("host under stress at +58 s for 5.0 s (io full 20.0 %, memory full 0.0 %, swap-in 400 pages/s)")
    why = render(v).split("## Why", 1)[1].strip().splitlines()[0]
    assert why.startswith("- `host_under_stress`: host under stress at +58 s")


def test_a_stall_on_a_calm_host_is_a_plain_generator_stall(tmp_path: Path) -> None:
    v = verdict(tmp_path, host_lines(T0 + 90_000, T0 + 95_000))
    assert [i["kind"] for i in jv.objs(v, "integrity")] == ["generator_stall"]
    # Stress the profile states a higher bound for is no stress.
    v = verdict(tmp_path / "b", host_lines(T0 + 58_000, T0 + 63_000, io_full_us=30_000, swapin=10),
                profile={"preflight": {"max_io_full_pct": 5, "max_swapin_pages_s": 50}})
    assert [i["kind"] for i in jv.objs(v, "integrity")] == ["generator_stall"]


def test_a_run_without_host_samples_judges_as_before(tmp_path: Path) -> None:
    recs = stalled_records()
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile={})
    v = compute(d, SCHEMAS)
    assert [i["kind"] for i in jv.objs(v, "integrity")] == ["generator_stall"]
    assert json.loads(json.dumps(v))["status"] == "invalid"


def test_a_run_is_refused_while_the_host_is_under_stress_and_samples_while_it_runs(tmp_path: Path) -> None:
    from hakit.content import discover
    from hakit.dryrun import simulated
    from hakit.lab import lab_from_env
    from hakit.paths import BASE_CONTENT
    from hakit.runner import Runner, RunRefused

    content = discover([BASE_CONTENT], SCHEMAS)
    proc = FakeProc()
    proc.avg10[("memory", "full")] = 9.0
    clock, fake = simulated([proc.rule])
    r = Runner(content, SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    with pytest.raises(RunRefused, match="host under stress: memory full pressure avg10 9 % is at or above 5 %"):
        r.run("kill-worker-smoke")
    proc.avg10[("memory", "full")] = 0.0
    run_dir = r.run("kill-worker-smoke")
    samples = read_jsonl(run_dir / HOST_FILE)
    span_s = (jv.number(samples[-1], "at_ms") - jv.number(samples[0], "at_ms")) / 1000
    # One sample a second while the runner waits, over the whole run.
    assert span_s >= 600 and len(samples) >= span_s * 0.9
