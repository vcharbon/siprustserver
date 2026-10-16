"""Every effect of the runner goes through an executor (ADR-0049 §6.3).

`RealExecutor` runs processes (in their own process group, ended whole on a
timeout: SIGTERM, then SIGKILL after a grace), HTTP requests and host reads. `FakeExecutor` records each call and
answers from scripted rules, so a whole profile replays on a fake clock.
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import urllib.error
import urllib.request
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Protocol


OCTETS = "application/octet-stream"


@dataclass(frozen=True)
class ProcResult:
    rc: int
    stdout: str
    stderr: str
    timed_out: bool = False


@dataclass(frozen=True)
class HttpResult:
    status: int
    body: str


class Executor(Protocol):
    def run(
        self,
        argv: list[str],
        *,
        env: Mapping[str, str] | None = None,
        cwd: Path | None = None,
        timeout_s: float = 60,
    ) -> ProcResult: ...

    def http(
        self, method: str, url: str, body: bytes | None = None, timeout_s: float = 10, content_type: str = OCTETS
    ) -> HttpResult: ...

    def read_text(self, path: Path) -> str | None: ...

    def disk_free_bytes(self, path: Path) -> int: ...


def _kill_group(pid: int, sig: signal.Signals = signal.SIGKILL) -> None:
    try:
        os.killpg(pid, sig)
    except ProcessLookupError:
        pass


class RealExecutor:
    """A step that overruns its timeout gets SIGTERM on its process group and
    `term_grace_s` to end (an executable stops what it started), then SIGKILL."""

    def __init__(self, term_grace_s: float = 5.0) -> None:
        self.term_grace_s = term_grace_s

    def run(
        self,
        argv: list[str],
        *,
        env: Mapping[str, str] | None = None,
        cwd: Path | None = None,
        timeout_s: float = 60,
    ) -> ProcResult:
        full_env = dict(os.environ)
        if env:
            full_env.update(env)
        try:
            proc = subprocess.Popen(
                argv,
                env=full_env,
                cwd=cwd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
        except OSError as e:
            return ProcResult(rc=127, stdout="", stderr=str(e))
        try:
            out, err = proc.communicate(timeout=timeout_s)
            return ProcResult(rc=proc.returncode, stdout=out, stderr=err)
        except subprocess.TimeoutExpired:
            _kill_group(proc.pid, signal.SIGTERM)
            try:
                out, err = proc.communicate(timeout=self.term_grace_s)
            except subprocess.TimeoutExpired:
                _kill_group(proc.pid)
                out, err = proc.communicate()
            return ProcResult(rc=proc.returncode, stdout=out, stderr=err, timed_out=True)
        except BaseException:
            _kill_group(proc.pid)
            proc.wait()
            raise

    def http(
        self, method: str, url: str, body: bytes | None = None, timeout_s: float = 10, content_type: str = OCTETS
    ) -> HttpResult:
        # Never urllib's default form content type: a form body is parsed, not ingested.
        headers = {"Content-Type": content_type} if body is not None else {}
        req = urllib.request.Request(url, data=body, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout_s) as resp:
                return HttpResult(status=resp.status, body=resp.read().decode(errors="replace"))
        except urllib.error.HTTPError as e:
            return HttpResult(status=e.code, body=e.read().decode(errors="replace"))
        except (urllib.error.URLError, OSError) as e:
            return HttpResult(status=0, body=str(e))

    def read_text(self, path: Path) -> str | None:
        try:
            return path.read_text(errors="replace")
        except OSError:
            return None

    def disk_free_bytes(self, path: Path) -> int:
        probe = path
        while not probe.exists() and probe != probe.parent:
            probe = probe.parent
        return shutil.disk_usage(probe).free


@dataclass(frozen=True)
class Call:
    kind: str  # "run", "http", "read"
    argv: list[str] = field(default_factory=lambda: [])
    env: dict[str, str] = field(default_factory=lambda: {})
    url: str = ""
    method: str = ""
    body: bytes | None = None
    path: Path | None = None


type Rule = Callable[[Call], ProcResult | HttpResult | str | None]


class FakeExecutor:
    """Records every call; the first rule that answers non-None decides the result.

    Without an answer a process exits 0 with no output, an HTTP request gets
    204, and a host read finds nothing.
    """

    def __init__(self, rules: list[Rule] | None = None, free_bytes: int = 1 << 40) -> None:
        self.rules: list[Rule] = list(rules or [])
        self.calls: list[Call] = []
        self.free_bytes = free_bytes

    def _answer(self, call: Call) -> ProcResult | HttpResult | str | None:
        self.calls.append(call)
        for rule in self.rules:
            got = rule(call)
            if got is not None:
                return got
        return None

    def run(
        self,
        argv: list[str],
        *,
        env: Mapping[str, str] | None = None,
        cwd: Path | None = None,
        timeout_s: float = 60,
    ) -> ProcResult:
        got = self._answer(Call(kind="run", argv=list(argv), env=dict(env or {})))
        return got if isinstance(got, ProcResult) else ProcResult(rc=0, stdout="", stderr="")

    def http(
        self, method: str, url: str, body: bytes | None = None, timeout_s: float = 10, content_type: str = OCTETS
    ) -> HttpResult:
        got = self._answer(Call(kind="http", url=url, method=method, body=body))
        return got if isinstance(got, HttpResult) else HttpResult(status=204, body="")

    def read_text(self, path: Path) -> str | None:
        got = self._answer(Call(kind="read", path=path))
        return got if isinstance(got, str) else None

    def disk_free_bytes(self, path: Path) -> int:
        return self.free_bytes

    def runs(self, prefix: list[str]) -> list[Call]:
        """The process calls whose argv starts with `prefix`."""
        return [c for c in self.calls if c.kind == "run" and c.argv[: len(prefix)] == prefix]
