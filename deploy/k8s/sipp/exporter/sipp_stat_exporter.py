#!/usr/bin/env python3
"""
SIPp stat-CSV -> Prometheus exporter (stdlib only).

SIPp (`-trace_stat -stf <file> -fd 1`) appends one `;`-separated row per flush
to a stat CSV. The first line is a header naming every column; cumulative
counters carry a `(C)` suffix, periodic ones `(P)`. This exporter parses the
header into a name->index map (robust to SIPp column drift across versions),
reads the LAST complete data row on each scrape, and exposes it at /metrics.
A scrape reads the first line and a bounded window at the end of the file, so
its memory is flat whatever the file size; stat_trim.py bounds the disk.

Headline series (labelled by scenario/role/job from the env):
  sipp_current_calls            gauge   concurrent established dialogs
  sipp_calls_created_total      counter TotalCallCreated
  sipp_successful_calls_total   counter SuccessfulCall(C)
  sipp_failed_calls_total       counter FailedCall(C)
  sipp_failed_total{cause=...}  counter one series per Failed* column
  sipp_retransmissions_total    counter Retransmissions(C)
  sipp_out_of_call_msgs_total   counter OutOfCallMsgs(C)
  sipp_dead_call_msgs_total     counter DeadCallMsgs(C)
  sipp_call_rate                gauge   CallRate(C)
  sipp_response_time_ms         gauge   ResponseTime1(C)
  sipp_call_length_ms           gauge   CallLength(C)
  sipp_up                       gauge   1 when the stat file is readable/fresh
  sipp_stat_file_allocated_bytes gauge  disk the stat file holds
  sipp_stat_trim_failed         gauge   1 while the trimmer's last attempt failed
  sipp_error_entries_total      counter entries of the error file (error_follow.py)
  sipp_unexpected_msgs_total{action,expecting,received}
                                counter unexpected-message entries: aborted or
                                        continued, the scenario step expected, the
                                        status code or method received

Env:
  SIPP_STAT_FILE  (default /stats/stat.csv)
  SIPP_SCENARIO   (default unknown)   -> label scenario=
  SIPP_ROLE       (default uac)       -> label role=
  SIPP_JOB        (default "")        -> label sipp_job=   (omitted if empty)
                                         (NOT `job`: that is reserved and gets
                                         overwritten by the vmagent scrape job)
  EXPORTER_PORT   (default 9035)
  SIPP_STAT_KEEP_BYTES (default 16 MiB) -> tail kept on disk by the trimmer;
                                         0 disables it (the file needs write access)
  SIPP_ERROR_FILE (default /stats/errors.log) -> SIPp's -error_file, followed
                                         across rotations; empty disables it
"""
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import error_follow
import stat_trim

STAT_FILE = os.environ.get("SIPP_STAT_FILE", "/stats/stat.csv")
SCENARIO = os.environ.get("SIPP_SCENARIO", "unknown")
ROLE = os.environ.get("SIPP_ROLE", "uac")
JOB = os.environ.get("SIPP_JOB", "")
PORT = int(os.environ.get("EXPORTER_PORT", "9035"))
KEEP_BYTES = int(os.environ.get("SIPP_STAT_KEEP_BYTES", str(16 * 1024 * 1024)))
ERROR_FILE = os.environ.get("SIPP_ERROR_FILE", "/stats/errors.log")

# Set by main() when enabled; read by every scrape.
TRIMMER = None
FOLLOWER = None

# Longest header line read; a stat row is about 0.5 KB, its header a few KB.
HEADER_MAX = 64 * 1024
# Bytes read at the end of the file per scrape: many rows, whatever the columns.
TAIL_WINDOW = 64 * 1024

# Failed* CSV column (cumulative) -> cause label on sipp_failed_total.
FAILURE_CAUSES = {
    "FailedCannotSendMessage(C)": "cannot_send",
    "FailedMaxUDPRetrans(C)": "max_udp_retrans",
    "FailedTcpConnect(C)": "tcp_connect",
    "FailedTcpClosed(C)": "tcp_closed",
    "FailedUnexpectedMessage(C)": "unexpected_msg",
    "FailedCallRejected(C)": "call_rejected",
    "FailedCmdNotSent(C)": "cmd_not_sent",
    "FailedRegexpDoesntMatch(C)": "regexp_doesnt_match",
    "FailedRegexpShouldntMatch(C)": "regexp_shouldnt_match",
    "FailedRegexpHdrNotFound(C)": "regexp_hdr_not_found",
    "FailedOutboundCongestion(C)": "congestion",
    "FailedTimeoutOnRecv(C)": "timeout_recv",
    "FailedTimeoutOnSend(C)": "timeout_send",
    "FailedTestDoesntMatch(C)": "test_doesnt_match",
    "FailedTestShouldntMatch(C)": "test_shouldnt_match",
    "FailedStrcmpDoesntMatch(C)": "strcmp_doesnt_match",
    "FailedStrcmpShouldntMatch(C)": "strcmp_shouldnt_match",
}


def base_labels():
    pairs = [("scenario", SCENARIO), ("role", ROLE)]
    if JOB:
        pairs.append(("sipp_job", JOB))
    return pairs


def _esc(v):
    return v.replace("\\", "\\\\").replace('"', '\\"')


def fmt(name, value, extra_labels=None):
    labels = base_labels() + list(extra_labels or [])
    body = ",".join(f'{k}="{_esc(v)}"' for k, v in labels)
    return f"{name}{{{body}}} {value}\n"


def parse_time_ms(cell):
    """SIPp time cells are HH:MM:SS:uuuuuu -> milliseconds (float)."""
    parts = cell.split(":")
    try:
        if len(parts) == 4:
            h, m, s, us = (int(p) for p in parts)
            return (h * 3600 + m * 60 + s) * 1000.0 + us / 1000.0
    except ValueError:
        pass
    return 0.0


def read_last_row(path):
    """Return (header_list, last_data_fields) or (None, None) if unreadable.

    The last data row is the last newline-terminated non-blank line in the final
    TAIL_WINDOW bytes: a row SIPp is still writing is skipped, and NUL bytes a
    punched hole reads as are stripped.
    """
    try:
        with open(path, "rb") as fh:
            head = fh.readline(HEADER_MAX)
            size = os.fstat(fh.fileno()).st_size
            start = max(len(head), size - TAIL_WINDOW)
            fh.seek(start)
            tail = fh.read(size - start)
    except OSError:
        return None, None
    if not head.endswith(b"\n"):
        return None, None
    lines = tail[:tail.rfind(b"\n") + 1].split(b"\n")
    if start > len(head):
        lines = lines[1:]  # the window may begin mid-row
    for raw in reversed(lines):
        line = raw.strip(b"\0").decode(errors="replace").strip()
        if line:
            return head.decode(errors="replace").strip().split(";"), line.split(";")
    return None, None


def render(path=STAT_FILE, trimmer=None, follower=None):
    out = []
    try:
        out.append(fmt("sipp_stat_file_allocated_bytes", os.stat(path).st_blocks * 512))
    except OSError:
        pass
    if trimmer is not None:
        out.append(fmt("sipp_stat_trim_failed", int(trimmer.failed)))
    if follower is not None:
        counts = follower.snapshot()
        out.append(fmt("sipp_error_entries_total", counts.entries))
        for labels, n in sorted(counts.unexpected.items()):
            out.append(fmt("sipp_unexpected_msgs_total", n, labels))
    header, row = read_last_row(path)
    if header is None:
        out.append(fmt("sipp_up", 0))
        return "".join(out)

    idx = {name: i for i, name in enumerate(header)}

    def num(col, cast=float):
        i = idx.get(col)
        if i is None or i >= len(row):
            return None
        cell = row[i].strip()
        if cell == "":
            return None
        try:
            return cast(cell)
        except ValueError:
            return None

    def emit(name, col, cast=float):
        v = num(col, cast)
        if v is not None:
            out.append(fmt(name, _trim(v)))

    out.append(fmt("sipp_up", 1))
    emit("sipp_current_calls", "CurrentCall", int)
    emit("sipp_calls_created_total", "TotalCallCreated", int)
    emit("sipp_successful_calls_total", "SuccessfulCall(C)", int)
    emit("sipp_failed_calls_total", "FailedCall(C)", int)
    emit("sipp_retransmissions_total", "Retransmissions(C)", int)
    emit("sipp_out_of_call_msgs_total", "OutOfCallMsgs(C)", int)
    emit("sipp_dead_call_msgs_total", "DeadCallMsgs(C)", int)
    emit("sipp_fatal_errors_total", "FatalErrors(C)", int)
    emit("sipp_warnings_total", "Warnings(C)", int)
    emit("sipp_call_rate", "CallRate(C)")

    # Time-formatted gauges.
    for name, col in (("sipp_response_time_ms", "ResponseTime1(C)"),
                      ("sipp_call_length_ms", "CallLength(C)")):
        i = idx.get(col)
        if i is not None and i < len(row) and row[i].strip():
            out.append(fmt(name, _trim(parse_time_ms(row[i].strip()))))

    # Per-cause failures (skip columns absent in this SIPp build).
    for col, cause in FAILURE_CAUSES.items():
        v = num(col, int)
        if v is not None:
            out.append(fmt("sipp_failed_total", v, [("cause", cause)]))

    return "".join(out)


def _trim(v):
    """Render ints without a trailing .0, floats compactly."""
    if isinstance(v, float) and v.is_integer():
        return str(int(v))
    return repr(v) if isinstance(v, float) else str(v)


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        if self.path.split("?")[0] not in ("/metrics", "/"):
            self.send_response(404)
            self.end_headers()
            return
        try:
            payload = render(STAT_FILE, TRIMMER, FOLLOWER).encode()
        except Exception as exc:  # never crash the scrape
            payload = (f"sipp_up 0\n# exporter error: {exc}\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; version=0.0.4")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):  # silence per-request logging
        pass


def main():
    srv = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
    print(f"sipp_stat_exporter: serving :{PORT}/metrics from {STAT_FILE} "
          f"(scenario={SCENARIO} role={ROLE} job={JOB or '-'})", file=sys.stderr)
    global TRIMMER, FOLLOWER
    if KEEP_BYTES > 0:
        TRIMMER = stat_trim.Trimmer(STAT_FILE, KEEP_BYTES).start()
    if ERROR_FILE:
        FOLLOWER = error_follow.start(ERROR_FILE)
    srv.serve_forever()


if __name__ == "__main__":
    main()
