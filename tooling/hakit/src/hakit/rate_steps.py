"""A profile's rate steps, applied while the load runs (a capacity ramp).

A stream's `steps` say that from `at_s` of the load on it offers `cps`. The
runner asks each step's generator to change its rate once the step is due and
appends what it did to the run's `steps.jsonl`: the stream, the step's
planned and applied instants, the rate, and whether the generator took it.
Only a stream that runs for the whole load has steps (the profile check
refuses them on a load incident's stream). A refused change is a
`generator_failure` integrity finding: the step's calls were not offered at
the rate the step says.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field

from .jsonval import Json
from .profile import Profile

# Applies `cps` to the running generator of a stream: True taken, False
# refused, None when the stream has no running generator.
type Apply = Callable[[str, float], bool | None]


@dataclass(frozen=True)
class Step:
    stream: str
    at_s: float
    cps: float


@dataclass
class RateSteps:
    steps: list[Step]
    done: int = 0
    applied: list[Json] = field(default_factory=lambda: [])

    @classmethod
    def of(cls, profile: Profile) -> RateSteps:
        steps = [Step(s.name, at_s, cps) for s in profile.streams for at_s, cps in s.steps]
        return cls(sorted(steps, key=lambda st: (st.at_s, st.stream)))

    def next_ms(self, load_start_ms: int) -> int | None:
        """The instant of the next step not applied yet."""
        if self.done >= len(self.steps):
            return None
        return load_start_ms + int(self.steps[self.done].at_s * 1000)

    def apply_due(self, load_start_ms: int, now_ms: int, apply: Apply) -> tuple[list[Json], list[Json]]:
        """Apply every step due at `now_ms`: the `steps.jsonl` lines and the
        integrity findings of the refused ones."""
        lines: list[Json] = []
        found: list[Json] = []
        while self.done < len(self.steps):
            st = self.steps[self.done]
            due = load_start_ms + int(st.at_s * 1000)
            if due > now_ms:
                break
            self.done += 1
            taken = apply(st.stream, st.cps)
            lines.append(
                {"stream": st.stream, "at_s": st.at_s, "due_ms": due, "applied_ms": now_ms, "cps": st.cps, "taken": taken}
            )
            if taken is False:
                found.append(
                    {
                        "kind": "generator_failure",
                        "stream": st.stream,
                        "at_ms": now_ms,
                        "detail": f"the generator refused the rate step to {st.cps:g} cps at {st.at_s:g} s",
                    }
                )
        self.applied += lines
        return lines, found
