"""What the runner asks of a stream's generator, whatever drives it (ADR-0049
§2.8, §4.5): start it, see it run, drain or kill it, read its counters and
its records, remove it.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from .profile import Stream


@dataclass(frozen=True)
class Tally:
    """A generator's own count of the calls it offered: placed or not, and
    those it failed itself. `None` where it could not be read."""

    calls: int | None
    not_placed: int = 0
    generator_failures: int | None = None


class Generator(Protocol):
    stream: Stream
    call_id_tag: str

    @property
    def records_path(self) -> Path: ...

    def max_concurrent(self) -> int: ...

    def start(self) -> str | None:
        """An error text when the generator refused to start."""
        ...

    def running(self) -> bool: ...

    def exit_code(self) -> int | None: ...

    def clean_exit(self, code: int | None) -> bool:
        """Whether a drained generator that exited with `code` ended as its
        drain promises."""
        ...

    def drain(self) -> bool:
        """Signal the generator to place no new call and exit once its calls
        end; whether the signal was delivered."""
        ...

    def kill(self) -> bool: ...

    def set_rate(self, cps: float) -> bool:
        """Offer `cps` from now on; whether the generator took it."""
        ...

    def tally(self) -> Tally: ...

    def local_records(self) -> int: ...

    def remove(self) -> None: ...
