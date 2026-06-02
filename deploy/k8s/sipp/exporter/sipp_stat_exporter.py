#!/usr/bin/env python3
"""
SIPp stat-CSV -> Prometheus exporter (stdlib only).

SIPp (`-trace_stat -stf <file> -fd 1`) appends one `;`-separated row per flush
to a stat CSV. The first line is a header naming every column; cumulative
counters carry a `(C)` suffix, periodic ones `(P)`. This exporter parses the
header into a name->index map (robust to SIPp column drift across versions),
reads the LAST data row on each scrape, and exposes it at /metrics.

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

Env:
  SIPP_STAT_FILE  (default /stats/stat.csv)
  SIPP_SCENARIO   (default unknown)   -> label scenario=
  SIPP_ROLE       (default uac)       -> label role=
  SIPP_JOB        (default "")        -> label sipp_job=   (omitted if empty)
                                         (NOT `job`: that is reserved and gets
                                         overwritten by the vmagent scrape job)
  EXPORTER_PORT   (default 9035)
"""
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STAT_FILE = os.environ.get("SIPP_STAT_FILE", "/stats/stat.csv")
SCENARIO = os.environ.get("SIPP_SCENARIO", "unknown")
ROLE = os.environ.get("SIPP_ROLE", "uac")
JOB = os.environ.get("SIPP_JOB", "")
PORT = int(os.environ.get("EXPORTER_PORT", "9035"))

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


def read_last_row():
    """Return (header_list, last_data_fields) or (None, None) if unreadable."""
    try:
        with open(STAT_FILE, "r", errors="replace") as fh:
            lines = [ln for ln in fh.read().splitlines() if ln.strip()]
    except OSError:
        return None, None
    if len(lines) < 2:
        return None, None
    header = lines[0].split(";")
    last = lines[-1].split(";")
    return header, last


def render():
    out = []
    header, row = read_last_row()
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
            payload = render().encode()
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
    srv.serve_forever()


if __name__ == "__main__":
    main()
