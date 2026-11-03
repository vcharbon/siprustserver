"""The `generator_stall` finding's account of a stall (ADR-0049 §3.12): the
signal that saw it, and where its source lies by the host's samples over it.

A gap in the call starts is a stall of its own length. A watchdog stall is
known only to lie between two statistics rows, and is named by those rows.
The source is the host when an interval over the stall is stressed (§6.6) or
its CPU contended (a task waiting for a CPU `CPU_SOME_PCT` of the time); the
generator's side when the host's intervals, none longer than
`MAX_INTERVAL_MS`, cover it calm: the generator itself, or a slow reader of
its record output, since a full record pipe blocks its writer; the host
unobserved otherwise.
"""

from __future__ import annotations

from collections.abc import Sequence
from datetime import UTC, datetime
from pathlib import Path

from .host import HOST_FILE, SAMPLE_MS, Interval, Thresholds, intervals
from .jsonl import read_jsonl
from .stalls import STARTS, Stall

# Stated, not fitted: a tenth of the time with a task waiting for a CPU is a
# contended host, whatever the io, memory and swap-in thresholds say.
CPU_SOME_PCT = 10.0
# An interval longer than two sample periods averages its pressure over too
# long a span to clear a stall inside it.
MAX_INTERVAL_MS = 2 * SAMPLE_MS


def _clock(ms: float) -> str:
    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%H:%M:%S.%f")[:-3]


def seen(st: Stall) -> str:
    """The stall as its signal saw it."""
    if st.source == STARTS:
        return f"a {st.ms:g} ms generator stall (a gap in its call starts)"
    return f"a generator stall its watchdog saw between the statistics rows at {_clock(st.at_ms)} and {_clock(st.until_ms)} UTC"


def _figures(over: Sequence[Interval]) -> str:
    cpu = max(iv.pct[("cpu", "some")] for iv in over)
    io = max(iv.pct[("io", "full")] for iv in over)
    mem = max(iv.pct[("memory", "full")] for iv in over)
    swapin = max(iv.swapin_pages_s for iv in over)
    return f"cpu some {cpu:.1f} %, io full {io:.1f} %, memory full {mem:.1f} %, swap-in {swapin:.0f} pages/s"


def source(st: Stall, ivs: Sequence[Interval], t: Thresholds) -> str:
    """Where the stall's source lies, with the host's worst figures over it."""
    over = [iv for iv in ivs if iv.start_ms < st.until_ms and iv.end_ms > st.at_ms]
    if not over:
        return "the host unobserved over it (no host sample)"
    figures = _figures(over)
    if any(iv.stressed(t) for iv in over):
        return f"the host under stress over it ({figures})"
    if any(iv.pct[("cpu", "some")] >= CPU_SOME_PCT for iv in over):
        return f"the host's CPU contended over it ({figures})"
    covered = over[0].start_ms <= st.at_ms and over[-1].end_ms >= st.until_ms
    if not covered or any(iv.end_ms - iv.start_ms > MAX_INTERVAL_MS for iv in over):
        return f"the host unobserved over it (its samples more than {MAX_INTERVAL_MS / 1000:g} s apart)"
    return (
        f"the host calm over it ({figures}): the generator's side, "
        "the generator itself or a slow reader of its record output"
    )


def stall_details(run_dir: Path, t: Thresholds, stalls: Sequence[Stall]) -> dict[Stall, str]:
    """Per stall, how it was seen and where its source lies."""
    ivs = intervals(read_jsonl(run_dir / HOST_FILE))
    return {st: f"{seen(st)}; {source(st, ivs, t)}" for st in stalls}
