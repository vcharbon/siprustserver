"""Before the load: the host facts, the profile's host limits, open events of an
earlier run, and calls the system under test still holds (ADR-0049 §6.5)."""

from __future__ import annotations

import os
from pathlib import Path

from .executor import Executor
from .jsonval import Json
from .profile import Preflight
from .verdict import read_jsonl

GIB = 1024**3


def host_facts(executor: Executor, results: Path) -> Json:
    facts: Json = {"cpus": os.cpu_count() or 1, "free_disk_gib": round(executor.disk_free_bytes(results) / GIB, 2)}
    meminfo = executor.read_text(Path("/proc/meminfo")) or ""
    kib: dict[str, int] = {}
    for line in meminfo.splitlines():
        name, _, rest = line.partition(":")
        parts = rest.split()
        if parts and parts[0].isdigit():
            kib[name] = int(parts[0])
    if "MemAvailable" in kib:
        facts["free_memory_gib"] = round(kib["MemAvailable"] * 1024 / GIB, 2)
    if "SwapTotal" in kib and "SwapFree" in kib:
        facts["swap_used_gib"] = round((kib["SwapTotal"] - kib["SwapFree"]) * 1024 / GIB, 2)
    load = (executor.read_text(Path("/proc/loadavg")) or "").split()
    if load:
        facts["load_1m"] = float(load[0])
    return facts


def _num(facts: Json, key: str) -> float | None:
    v = facts.get(key)
    return float(v) if isinstance(v, (int, float)) else None


def limit_errors(limits: Preflight, facts: Json) -> list[str]:
    errs: list[str] = []
    mem, swap, disk, load = (
        _num(facts, "free_memory_gib"),
        _num(facts, "swap_used_gib"),
        _num(facts, "free_disk_gib"),
        _num(facts, "load_1m"),
    )
    cpus = _num(facts, "cpus") or 1.0
    if limits.min_free_memory_gib is not None and (mem is None or mem < limits.min_free_memory_gib):
        errs.append(f"free memory {mem} GiB is below {limits.min_free_memory_gib}")
    if limits.max_swap_used_gib is not None and swap is not None and swap > limits.max_swap_used_gib:
        errs.append(f"swap used {swap} GiB is above {limits.max_swap_used_gib}")
    if limits.min_free_disk_gib is not None and (disk is None or disk < limits.min_free_disk_gib):
        errs.append(f"free disk {disk} GiB is below {limits.min_free_disk_gib}")
    if limits.max_load_per_cpu is not None and load is not None and load / cpus > limits.max_load_per_cpu:
        errs.append(f"load per cpu {load / cpus:.2f} is above {limits.max_load_per_cpu}")
    return errs


def open_events(run_dir: Path) -> list[str]:
    """Events of a run directory with an incident that injected and never healed.

    `hakit cleanup` leaves a `cleaned` marker once it healed them all. A
    detected event heals nothing and is never open.
    """
    if (run_dir / "cleaned").is_file():
        return []
    injected: set[tuple[str, str]] = set()
    healed: set[tuple[str, str]] = set()
    detected: set[str] = set()
    for line in read_jsonl(run_dir / "events.jsonl"):
        if line.get("kind") == "event" and line.get("source") == "detected":
            detected.add(str(line.get("event")))
        if line.get("kind") != "phase" or line.get("event") in detected:
            continue
        key = (str(line.get("event")), str(line.get("step")))
        if line.get("phase") == "inject":
            injected.add(key)
        elif line.get("phase") == "healed":
            healed.add(key)
    return sorted({ev for ev, _ in injected - healed})


def earlier_open_events(results: Path) -> list[str]:
    if not results.is_dir():
        return []
    out: list[str] = []
    for d in sorted(p for p in results.iterdir() if p.is_dir()):
        out += [f"{d.name}/{ev}" for ev in open_events(d)]
    return out
