"""The base content keeps the spike rule (ADR-0049 §2.11): an overload refusal
(503) is budgeted only in the window of an incident that models a loop or a
flood, never for a spike, a starved CPU or any fault."""

from __future__ import annotations

from rundir import CONTENT

from hakit.schedule import plan

# The base's incidents that model catastrophic load.
FLOODS = {"abuse"}


def test_a_503_is_budgeted_only_for_a_loop_or_a_flood() -> None:
    for profile in CONTENT.profiles.values():
        for ev in plan(profile, CONTENT.incidents):
            budget = CONTENT.budgets[ev.spec.budget]
            codes = {a.code for a in budget.setup_failed + budget.unexpected_ending}
            if 503 in codes:
                assert set(ev.spec.incidents) <= FLOODS, f"{profile.name} {ev.event_id}: budget {budget.name} allows a 503"
