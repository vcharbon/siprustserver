"""Before the load: host limits and events an earlier run left open."""

from __future__ import annotations

from pathlib import Path

from kit import write_jsonl

from hakit.dryrun import simulated
from hakit.preflight import earlier_open_events, host_facts, limit_errors
from hakit.profile import Preflight
from hakit.windows import phase_line


def test_host_facts_and_limits(tmp_path: Path) -> None:
    _, fake = simulated()
    facts = host_facts(fake, tmp_path)
    assert facts["free_memory_gib"] == 64.0 and facts["swap_used_gib"] == 0.0 and facts["load_1m"] == 0.0
    assert limit_errors(Preflight(4, 1, 10, 1), facts) == []
    assert limit_errors(Preflight(128, None, None, None), facts) == ["free memory 64.0 GiB is below 128"]


def test_an_event_injected_and_never_healed_is_open_until_cleanup(tmp_path: Path) -> None:
    run = tmp_path / "r1"
    run.mkdir()
    write_jsonl(run / "events.jsonl", [phase_line("001-k", "k", "inject", 1), phase_line("001-k", "k", "failed", 2)])
    assert earlier_open_events(tmp_path) == ["r1/001-k"]
    write_jsonl(
        run / "events.jsonl",
        [phase_line("001-k", "k", "inject", 1), phase_line("001-k", "k", "heal", 2), phase_line("001-k", "k", "healed", 3)],
    )
    assert earlier_open_events(tmp_path) == []
    write_jsonl(run / "events.jsonl", [phase_line("001-k", "k", "inject", 1)])
    (run / "cleaned").write_text("")
    assert earlier_open_events(tmp_path) == []
