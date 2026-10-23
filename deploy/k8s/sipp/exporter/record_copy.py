"""
Carries the call-outcome records a SIPp UAC writes into a FIFO (its -log_file)
to the record file on disk, through a bounded in-memory buffer.

SIPp writes and flushes each record from its one thread: a write into the FIFO
returns once the pipe holds it, whatever the disk does. The reader thread
moves the complete lines out of the pipe into the buffer and never touches the
disk; the writer thread appends the buffer to the record file. The memory held
is the buffer, never the run's records. Lines that do not fit the buffer (a
disk slower than SIPp for longer than the buffer holds) or that fail to write
are counted lost, never written in part.

The status file (JSON: complete, lines, lost) is rewritten by a third thread
each time `lost` grows, and once SIPp closed the FIFO and every line is on
disk or lost (`complete`). `lines` counts the lines on disk, a last line SIPp
did not end included. A status that never says complete (the twin died, the
disk stalled past the run) is the runner's to report.
"""
import collections
import fcntl
import json
import os
import threading

CHUNK = 64 * 1024
# Pipe capacity asked for the FIFO: SIPp blocks only once it is full.
PIPE_BYTES = 1 << 20
F_SETPIPE_SZ = getattr(fcntl, "F_SETPIPE_SZ", 1031)


class RecordCopy:
    """feed() and close() are the reader's side, write_all() the writer's,
    report_all() the status file's."""

    def __init__(self, dst, status, buffer_bytes):
        self.dst, self.status, self.cap = dst, status, buffer_bytes
        self._blocks = collections.deque()
        self._held = 0
        self._partial = b""
        # Set while the rest of a line longer than the buffer is dropped.
        self._skipping = False
        # Set while the record file ends in a line a failed write tore.
        self._torn = False
        self._closed = False
        self._complete = False
        self._lines = 0
        self._lost = 0
        self._cond = threading.Condition()

    def held(self):
        with self._cond:
            return self._held + len(self._partial)

    def lost(self):
        with self._cond:
            return self._lost

    def _queue(self, block, lines):
        """Called with the lock held."""
        if self._held + len(block) > self.cap:
            self._lost += lines
        else:
            self._blocks.append((block, lines))
            self._held += len(block)
        self._cond.notify_all()

    def feed(self, chunk):
        """Queue the complete lines of `chunk`; a line without its end waits
        for it, and one longer than the buffer is lost whole."""
        with self._cond:
            if self._skipping:
                end = chunk.find(b"\n")
                if end < 0:
                    return
                chunk, self._skipping = chunk[end + 1:], False
            data = self._partial + chunk
            cut = data.rfind(b"\n") + 1
            self._partial = data[cut:]
            if cut:
                self._queue(data[:cut], data.count(b"\n", 0, cut))
            if len(self._partial) > self.cap:
                self._partial, self._skipping = b"", True
                self._lost += 1
                self._cond.notify_all()

    def close(self):
        """SIPp closed the FIFO: a last line it did not end is queued as it is."""
        with self._cond:
            if self._partial:
                self._queue(self._partial, 1)
                self._partial = b""
            self._closed = True
            self._cond.notify_all()

    def write_all(self):
        """Append the buffer to the record file until the FIFO is closed and
        the buffer empty, then mark the copy complete."""
        fd = os.open(self.dst, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
        try:
            while True:
                with self._cond:
                    while not self._blocks and not self._closed:
                        self._cond.wait()
                    if not self._blocks:
                        self._complete = True
                        self._cond.notify_all()
                        return
                    block, lines = self._blocks.popleft()
                self._write(fd, block, lines)
        finally:
            os.close(fd)

    def _write(self, fd, block, lines):
        """Append one block; after a failed write, the lines written whole are
        on disk, a torn last one too (ended by the next write), the rest lost."""
        ok = not self._torn or _write_all(fd, b"\n")[1]
        self._torn = self._torn and not ok
        n, ok = _write_all(fd, block) if ok else (0, False)
        whole = lines if ok else block.count(b"\n", 0, n)
        torn = not ok and n > 0 and block[n - 1:n] != b"\n"
        self._torn = self._torn or torn
        with self._cond:
            self._held -= len(block)
            self._lines += whole + torn
            self._lost += lines - whole - torn
            self._cond.notify_all()

    def report_all(self):
        """Rewrite the status file each time `lost` grows, and once complete;
        its own thread, so a writer stuck on the disk never holds the count back."""
        reported = None
        while True:
            with self._cond:
                while (self._lost, self._complete) == reported:
                    self._cond.wait()
                doc = {"complete": self._complete, "lines": self._lines, "lost": self._lost}
                reported = (self._lost, self._complete)
            self._report(doc)
            if doc["complete"]:
                return

    def _report(self, doc):
        if self.status is None:
            return
        tmp = self.status + ".tmp"
        try:
            with open(tmp, "w") as fh:
                json.dump(doc, fh)
            os.replace(tmp, self.status)
        except OSError:
            pass

    def read_fifo(self, fifo):
        """Read the FIFO until SIPp closes it; the open waits for SIPp's."""
        fd = os.open(fifo, os.O_RDONLY)
        try:
            try:
                fcntl.fcntl(fd, F_SETPIPE_SZ, PIPE_BYTES)
            except OSError:
                pass
            while True:
                chunk = os.read(fd, CHUNK)
                if not chunk:
                    break
                self.feed(chunk)
        finally:
            os.close(fd)
            self.close()


def start(fifo, dst, status, buffer_bytes):
    """A copier whose reader, writer and reporter run on daemon threads."""
    copy = RecordCopy(dst, status, buffer_bytes)
    threading.Thread(target=copy.read_fifo, args=(fifo,), name="record-read", daemon=True).start()
    threading.Thread(target=copy.write_all, name="record-write", daemon=True).start()
    threading.Thread(target=copy.report_all, name="record-report", daemon=True).start()
    return copy


def _write_all(fd, data):
    """The bytes written before an error, and whether all were."""
    view, n = memoryview(data), 0
    try:
        while n < len(data):
            n += os.write(fd, view[n:])
        return n, True
    except OSError:
        return n, False
