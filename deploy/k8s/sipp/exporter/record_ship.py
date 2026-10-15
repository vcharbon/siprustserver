"""
Ships the call-outcome records a SIPp UAC writes (one JSON line per call, its
`<log>` action into -log_file) to VictoriaLogs, and counts them.

Each record goes out as one /insert/jsonline line: the record verbatim in
`_msg`, `_time` from its `ended_at_ms`, and `run`, `source`, `stream` (the
stream fields), `shape`, `ending`, `call_id` beside it. A line that is not a
JSON object still ships, with `unparsed` set, so a reader sees it and counts
it. Lines that fail to post stay queued and go out with the next poll; nothing
is dropped while the process lives.

Counters: records read by ending, records shipped, failed posts, queued lines.
"""
import collections
import json
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone

CHUNK = 64 * 1024
BATCH = 1000
INSERT = "/insert/jsonline?_stream_fields=run,source,stream&_msg_field=_msg&_time_field=_time"

Counts = collections.namedtuple("Counts", "by_ending shipped failed_posts pending")


def _iso(ms):
    return datetime.fromtimestamp(ms / 1000, timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def to_line(raw, run, stream, shape):
    """The VictoriaLogs line of one record line, and its ending (None when unparsed)."""
    try:
        rec = json.loads(raw)
        if not isinstance(rec, dict):
            raise ValueError("not an object")
    except ValueError:
        out = {"_msg": raw, "_time": _iso(time.time() * 1000), "run": run, "source": "sipp",
               "stream": stream, "shape": shape, "unparsed": "true"}
        return json.dumps(out), None
    ended = rec.get("ended_at_ms")
    out = {
        "_msg": raw,
        "_time": _iso(ended if isinstance(ended, (int, float)) else time.time() * 1000),
        "run": str(rec.get("run", run)),
        "source": str(rec.get("source", "sipp")),
        "stream": str(rec.get("stream", stream)),
        "shape": str(rec.get("shape", shape)),
        "ending": str(rec.get("ending", "")),
        "call_id": str(rec.get("call_id", "")),
    }
    return json.dumps(out), out["ending"]


def http_post(url, body):
    req = urllib.request.Request(url, data=body, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return 200 <= resp.status < 300
    except (urllib.error.URLError, OSError):
        return False


class RecordShipper:
    """Follows one record file from its start; poll() reads and ships what was
    appended, snapshot() returns the counters. Safe across threads."""

    def __init__(self, path, base_url, run="", stream="", shape="", post=http_post):
        self.path = path
        self.url = base_url.rstrip("/") + INSERT
        self.run, self.stream, self.shape = run, stream, shape
        self.post = post
        self._fh = None
        self._partial = b""
        self._queue = []
        self._by_ending = collections.Counter()
        self._shipped = 0
        self._failed = 0
        self._lock = threading.Lock()

    def poll(self):
        with self._lock:
            if self._fh is None:
                try:
                    self._fh = open(self.path, "rb")
                except FileNotFoundError:
                    return
            while True:
                chunk = self._fh.read(CHUNK)
                if not chunk:
                    break
                data = self._partial + chunk
                lines = data.split(b"\n")
                self._partial = lines.pop()
                for raw in lines:
                    text = raw.decode(errors="replace").strip()
                    if not text:
                        continue
                    line, ending = to_line(text, self.run, self.stream, self.shape)
                    self._by_ending[ending or "unparsed"] += 1
                    self._queue.append(line)
            while self._queue:
                batch = self._queue[:BATCH]
                if not self.post(self.url, ("\n".join(batch) + "\n").encode()):
                    self._failed += 1
                    return
                del self._queue[:len(batch)]
                self._shipped += len(batch)

    def snapshot(self):
        with self._lock:
            return Counts(dict(self._by_ending), self._shipped, self._failed, len(self._queue))


def start(path, base_url, run="", stream="", shape="", interval=1.0):
    """A shipper polled every `interval` seconds on a daemon thread."""
    shipper = RecordShipper(path, base_url, run, stream, shape)

    def loop():
        while True:
            try:
                shipper.poll()
            except Exception:  # the reporting path never takes the exporter down
                pass
            time.sleep(interval)

    threading.Thread(target=loop, name="record-ship", daemon=True).start()
    return shipper
