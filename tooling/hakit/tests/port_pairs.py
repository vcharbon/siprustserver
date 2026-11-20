"""Pairs of adjacent loopback UDP ports reserved box-wide for a SIPp pairing.

A port probed free is bound by SIPp only later, and pairings run in parallel in
several test processes at once. A reservation therefore holds an exclusive
`flock` on one file per port in a fixed directory, whatever a process's TMPDIR,
for as long as the pairing runs, and draws its ports below the kernel's
ephemeral range, which no `bind(0)` on the box is handed. The lock files stay:
at most one per port of the range.
"""

from __future__ import annotations

import fcntl
import random
import socket
from collections.abc import Generator
from contextlib import ExitStack, contextmanager
from pathlib import Path
from typing import IO

LOCK_DIR = Path("/tmp/hakit-sipp-ports")
LOWEST_PORT = 20000


def ephemeral_floor() -> int:
    """The lowest port the kernel hands a `bind(0)`."""
    try:
        return int(Path("/proc/sys/net/ipv4/ip_local_port_range").read_text().split()[0])
    except (OSError, ValueError, IndexError):
        return 32768


@contextmanager
def reserved_port_pair(low: int = LOWEST_PORT, high: int | None = None) -> Generator[int]:
    """The first of two adjacent ports in [low, high), both free on loopback
    when taken and held by no other reservation on the box until the block
    exits; `high` defaults to the ephemeral floor. Raises RuntimeError when
    no pair in the range is free."""
    top = ephemeral_floor() if high is None else high
    firsts = list(range(low, top - 1))
    if not firsts:
        raise RuntimeError(f"no UDP port pair fits in [{low}, {top})")
    start = random.randrange(len(firsts))
    for port in firsts[start:] + firsts[:start]:
        with ExitStack() as held:
            if all(_hold(held, p) for p in (port, port + 1)):
                yield port
                return
    raise RuntimeError(f"no free UDP port pair in [{low}, {top})")


def _hold(held: ExitStack, port: int) -> bool:
    """Takes the box-wide lock of `port` into `held` when no other reservation
    holds it and the port binds on loopback."""
    LOCK_DIR.mkdir(exist_ok=True)
    try:
        lock: IO[str] = held.enter_context((LOCK_DIR / str(port)).open("a"))
    except PermissionError:
        return False
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return False
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        try:
            probe.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True
