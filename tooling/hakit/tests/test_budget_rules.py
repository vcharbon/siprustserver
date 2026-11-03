"""The base content keeps the spike rule (ADR-0049 §2.11): an overload refusal
(503) is budgeted only in the window of an incident that models a loop or a
flood, never for a spike, a starved CPU or any fault; a 503 with a
Retry-After also in the window of a front proxy cut off its workers or a plane
(ADR-0056 §3)."""

from __future__ import annotations

from rundir import CONTENT

from hakit.schedule import plan

# The base's incidents that model catastrophic load.
FLOODS = {"abuse"}
# The base's incidents that cut a front proxy off its workers or a plane: a
# proxy reading no worker alive answers 503 with a Retry-After until its
# node's routes are withdrawn (ADR-0056 §3).
CUTS = {"cut_proxy_workers", "cut_external_plane", "backend_outage"}


def test_a_503_is_budgeted_only_for_a_loop_or_a_flood() -> None:
    for profile in CONTENT.profiles.values():
        for ev in plan(profile, CONTENT.incidents):
            budget = CONTENT.budgets[ev.spec.budget]
            allowances = budget.setup_failed + budget.unexpected_ending
            if any(a.code == 503 and a.retry_after is not True for a in allowances):
                assert set(ev.spec.incidents) <= FLOODS, f"{profile.name} {ev.event_id}: budget {budget.name} allows a 503"
            if any(a.code == 503 for a in allowances):
                assert set(ev.spec.incidents) <= FLOODS | CUTS, f"{profile.name} {ev.event_id}: budget {budget.name} allows a 503"
