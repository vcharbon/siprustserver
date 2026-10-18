"""The real executor's effects: a process group killed on its timeout, a post never sent as a form."""

from __future__ import annotations

import http.server
import subprocess
import threading
import time
from pathlib import Path

import pytest

from hakit.executor import RealExecutor


def test_a_post_is_never_a_form() -> None:
    seen: dict[str, str] = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802
            seen["type"] = str(self.headers.get("Content-Type"))
            self.rfile.read(int(self.headers["Content-Length"]))
            self.send_response(204)
            self.end_headers()

        def log_message(self, format: str, *args: object) -> None:  # noqa: A002
            pass

    srv = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.handle_request, daemon=True).start()
    try:
        res = RealExecutor().http("POST", f"http://127.0.0.1:{srv.server_port}/api/v1/import/prometheus", b"m 1\n")
    finally:
        srv.server_close()
    assert res.status == 204
    assert seen["type"] == "application/octet-stream"


def test_a_body_cut_mid_chunk_is_a_failed_request() -> None:
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            self.wfile.write(b"10\r\n0123456789")
            self.close_connection = True

        def log_message(self, format: str, *args: object) -> None:  # noqa: A002
            pass

    srv = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.handle_request, daemon=True).start()
    try:
        res = RealExecutor().http("GET", f"http://127.0.0.1:{srv.server_port}/select/logsql/query")
    finally:
        srv.server_close()
    assert res.status == 0 and "IncompleteRead" in res.body


def test_a_step_overrunning_its_timeout_is_ended_with_its_children() -> None:
    t0 = time.monotonic()
    res = RealExecutor().run(["sh", "-c", "sleep 30 & sleep 30"], timeout_s=0.5)
    assert res.timed_out and res.rc == -15
    assert time.monotonic() - t0 < 5


def test_an_overrunning_step_gets_sigterm_and_a_grace_to_stop_what_it_started(tmp_path: Path) -> None:
    mark = tmp_path / "stopped"
    script = f"trap 'echo cleaned > {mark}; exit 3' TERM; while :; do sleep 0.05; done"
    res = RealExecutor(term_grace_s=5).run(["bash", "-c", script], timeout_s=0.5)
    assert res.timed_out and res.rc == 3 and mark.read_text() == "cleaned\n"


def test_a_step_ignoring_sigterm_is_killed_after_the_grace() -> None:
    t0 = time.monotonic()
    res = RealExecutor(term_grace_s=0.5).run(["bash", "-c", "trap '' TERM; while :; do sleep 0.05; done"], timeout_s=0.5)
    assert res.timed_out and res.rc == -9
    assert time.monotonic() - t0 < 5


def test_an_interruption_during_a_step_kills_its_process_group(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    pidfile = tmp_path / "child.pid"

    def interrupted(self: subprocess.Popen[str], input: str | None = None, timeout: float | None = None) -> tuple[str, str]:  # noqa: A002
        deadline = time.monotonic() + 5
        while not pidfile.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        raise KeyboardInterrupt

    monkeypatch.setattr(subprocess.Popen, "communicate", interrupted)
    t0 = time.monotonic()
    with pytest.raises(KeyboardInterrupt):
        RealExecutor().run(["sh", "-c", f"sleep 30 & echo $! > {pidfile}; wait"], timeout_s=60)
    assert time.monotonic() - t0 < 10  # the step's children died with it, nothing was waited out
    child = int(pidfile.read_text())
    deadline = time.monotonic() + 5
    while Path(f"/proc/{child}").exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    assert not Path(f"/proc/{child}").exists()
