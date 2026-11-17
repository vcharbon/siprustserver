"""A SIPp pairing's port pair is reserved box-wide, across test processes."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

from port_pairs import ephemeral_floor, reserved_port_pair

CLAIM = """
import sys
from port_pairs import reserved_port_pair
low = int(sys.argv[1])
try:
    with reserved_port_pair(low, low + 2) as port:
        print(port)
except RuntimeError:
    print("refused")
"""


def _claim_in_another_process(low: int) -> str:
    """What another process gets when it asks for the one pair starting at `low`."""
    tests = str(Path(__file__).parent)
    run = subprocess.run([sys.executable, "-c", CLAIM, str(low)], capture_output=True, text=True, check=True, cwd=tests, timeout=30)
    return run.stdout.strip()


def test_a_reserved_pair_is_refused_to_another_process_until_released() -> None:
    with reserved_port_pair() as port:
        assert port + 1 < ephemeral_floor()
        assert _claim_in_another_process(port) == "refused"
    assert _claim_in_another_process(port) == str(port)
