"""
Counts the entries of a SIPp error file (-trace_err -error_file <path>) over the
whole run, while SIPp rotates it (-ringbuffer_files / -ringbuffer_size).

SIPp rotates by renaming the file away and opening a new one at the same path,
and removes the oldest rotated file past -ringbuffer_files. The follower keeps
the file it reads open: on a rotation it reads the old file to its end through
that descriptor (renamed or removed alike), then opens the new one from its
start. A file SIPp truncates in place is read again from its start. Two
rotations between two polls lose the middle file; at a 1 s poll that needs more
than -ringbuffer_size bytes of errors a second.

Counters restart at 0 with the process and then count the current file again:
a reader takes them as reset-aware counters (increase()), and a restart can
count the current file's entries twice, never less than once.
"""
import collections
import os
import re
import threading
import time

# Every entry starts with SIPp's timestamp: "YYYY-MM-DD\tHH:MM:SS.us\tepoch.us: ".
ENTRY = re.compile(rb"\d{4}-\d{2}-\d{2}\t\d{2}:\d{2}:\d{2}\.\d+\t\d+\.\d+: ")
# The first line of an unexpected-message entry; the received message follows.
UNEXPECTED = re.compile(
    rb"(Aborting|Continuing) call on unexpected message for Call-Id '.*?': "
    rb"while (?:expecting '([^']*)'|(sending|pausing|expecting command|in message type)"
    rb")[^,]*, received '(?:SIP/2\.0 (\d{3})|([A-Za-z]+) )")
ACTIONS = {b"Aborting": "aborted", b"Continuing": "continued"}

CHUNK = 64 * 1024
# A partial line longer than this is dropped: no entry's first line is that long.
LINE_MAX = 64 * 1024

Counts = collections.namedtuple("Counts", "entries unexpected")


class ErrorFollower:
    """Follows one SIPp error file; poll() reads what was appended, snapshot()
    returns the counters. Safe to poll and snapshot from different threads."""

    def __init__(self, path):
        self.path = path
        self._fh = None
        self._partial = b""
        self._entries = 0
        self._unexpected = collections.Counter()
        self._lock = threading.Lock()

    def poll(self):
        with self._lock:
            if self._fh is None:
                self._open()
                if self._fh is None:
                    return
            self._drain()
            try:
                st = os.stat(self.path)
            except FileNotFoundError:
                return  # renamed away, the new file not yet created
            cur = os.fstat(self._fh.fileno())
            if (st.st_dev, st.st_ino) != (cur.st_dev, cur.st_ino):
                self._drain()
                self._close()
                self._open()
                if self._fh is not None:
                    self._drain()
            elif st.st_size < self._fh.tell():
                self._fh.seek(0)
                self._partial = b""
                self._drain()

    def snapshot(self):
        with self._lock:
            return Counts(self._entries, dict(self._unexpected))

    def close(self):
        with self._lock:
            self._close()

    def _open(self):
        try:
            self._fh = open(self.path, "rb")
        except FileNotFoundError:
            self._fh = None
        self._partial = b""

    def _close(self):
        if self._fh is not None:
            self._fh.close()
            self._fh = None

    def _drain(self):
        while True:
            chunk = self._fh.read(CHUNK)
            if not chunk:
                return
            lines = (self._partial + chunk).split(b"\n")
            self._partial = lines.pop()
            if len(self._partial) > LINE_MAX:
                self._partial = b""
            for line in lines:
                self._count(line)

    def _count(self, line):
        m = ENTRY.match(line)
        if not m:
            return
        self._entries += 1
        u = UNEXPECTED.match(line, m.end())
        if u:
            action, recv_step, state, code, method = u.groups()
            key = (("action", ACTIONS[action]),
                   ("expecting", (recv_step or state).decode(errors="replace")),
                   ("received", (code or method).decode(errors="replace")))
            self._unexpected[key] += 1


def start(path, interval_s=1.0):
    """Poll `path` every `interval_s` on a daemon thread; returns the follower."""
    follower = ErrorFollower(path)

    def loop():
        while True:
            time.sleep(interval_s)
            try:
                follower.poll()
            except OSError:
                pass  # an unreadable file is retried at the next poll

    threading.Thread(target=loop, daemon=True, name="error-follow").start()
    return follower
