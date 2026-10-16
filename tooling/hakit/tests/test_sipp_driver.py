"""The SIPp command line a stream runs: the scenario's key defaults under the stream's params."""

from __future__ import annotations

import math
from pathlib import Path

from hakit.executor import FakeExecutor
from hakit.lab import lab_from_env
from hakit.model import Scalar
from hakit.profile import Stream
from hakit.sipp_driver import TXN_BOUND_S, SippStream

SCENARIO = """<?xml version="1.0" encoding="ISO-8859-1" ?>
<!--
  hakit-key: reinvites=2
  hakit-key: tone=low
  hakit-key: hold_ms=1
-->
<scenario name="k"><send><![CDATA[INVITE sip:x@[remote_ip] SIP/2.0
X-K: [reinvites] [tone] [hold_ms]
]]></send></scenario>
"""


def stream(params: dict[str, Scalar]) -> Stream:
    return Stream("s", "sipp", "run", "basic", "k.xml", 2.0, 10.0, 0.0, 20.0, None, params)


def driver(tmp_path: Path, params: dict[str, Scalar]) -> SippStream:
    p = tmp_path / "k.xml"
    p.write_text(SCENARIO)
    return SippStream("r", stream(params), 0, p, tmp_path, lab_from_env({}), FakeExecutor())


def keys(argv: list[str]) -> list[tuple[str, str]]:
    return [(argv[i + 1], argv[i + 2]) for i, a in enumerate(argv) if a == "-key"]


def test_defaults_come_first_and_the_streams_params_override_them(tmp_path: Path) -> None:
    """SIPp keeps the last `-key` of a name: a param after its default wins."""
    k = keys(driver(tmp_path, {"tone": "high"}).sipp_argv())
    assert k.index(("tone", "low")) < k.index(("tone", "high"))
    assert ("reinvites", "2") in k
    # The runner's own keys come from the stream, never from a scenario default.
    assert ("hold_ms", "1") not in k and ("hold_ms", "20000") in k


def test_the_call_limit_and_the_backstop_read_the_scenarios_defaults(tmp_path: Path) -> None:
    """Two re-INVITEs by default add two transaction bounds to the worst call."""
    d = driver(tmp_path, {})
    worst = TXN_BOUND_S + (20 + 2 * TXN_BOUND_S) + TXN_BOUND_S
    assert d.max_concurrent() == int(math.ceil(2.0 * worst)) + 10
    assert d.recv_timeout_ms() == (worst + 60) * 1000
    assert driver(tmp_path, {"reinvites": 0}).max_concurrent() == int(math.ceil(2.0 * (TXN_BOUND_S + 32 + TXN_BOUND_S))) + 10
