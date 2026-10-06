"""
Bounds the disk a SIPp stat CSV holds while SIPp keeps appending to it.

SIPp has no rotation for `-trace_stat`: it writes the file through one stream at
its own offset for the whole run. The trimmer punches a hole
(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) between the header line and the last
`keep_bytes`: the apparent size and the writer's offset are unchanged, the
blocks in between are released and read back as NUL bytes. The header and the
recent rows stay readable, so the exporter's tail read is unaffected.

Linux only; where hole punching fails (the filesystem, a read-only mount) `trim`
raises OSError and the Trimmer retries on a backoff, exposing `failed`.
"""
import ctypes
import ctypes.util
import errno
import os
import sys
import threading
import time

FALLOC_FL_KEEP_SIZE = 0x01
FALLOC_FL_PUNCH_HOLE = 0x02

# Seconds between two trims; a stat row a second grows the file by ~40 MB a day.
INTERVAL_S = 60
MAX_BACKOFF_S = 3600
HEADER_MAX = 64 * 1024

_libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
_fallocate = getattr(_libc, "fallocate64", None) or getattr(_libc, "fallocate", None)
if _fallocate is not None:
    _fallocate.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int64, ctypes.c_int64]
    _fallocate.restype = ctypes.c_int


def trim(path, keep_bytes):
    """Release the blocks between the header and the last `keep_bytes`.

    Acts only once more than 2 x `keep_bytes` past the header are allocated, so a
    file is trimmed once per `keep_bytes` of growth. Returns the bytes punched
    (0 when under the bound). Raises OSError when the file or its filesystem
    cannot punch holes.
    """
    if _fallocate is None:
        raise OSError(errno.ENOSYS, "fallocate is not available")
    fd = os.open(path, os.O_RDWR)
    try:
        st = os.fstat(fd)
        head = os.pread(fd, HEADER_MAX, 0)
        header_end = head.find(b"\n") + 1
        if header_end == 0:
            return 0
        if st.st_blocks * 512 <= header_end + 2 * keep_bytes:
            return 0
        blk = st.st_blksize
        start = -(-header_end // blk) * blk
        end = (st.st_size - keep_bytes) // blk * blk
        if end <= start:
            return 0
        flags = FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE
        if _fallocate(fd, flags, start, end - start) != 0:
            err = ctypes.get_errno()
            raise OSError(err, os.strerror(err), path)
        return end - start
    finally:
        os.close(fd)


class Trimmer:
    """Trims one file every `interval_s`; after a failure it retries on a doubling
    backoff capped at `max_backoff_s`, and `failed` holds until a trim succeeds."""

    def __init__(self, path, keep_bytes, interval_s=INTERVAL_S, max_backoff_s=MAX_BACKOFF_S):
        self.path = path
        self.keep_bytes = keep_bytes
        self.interval_s = interval_s
        self.max_backoff_s = max_backoff_s
        self.failed = False
        self._delay = interval_s

    def run_once(self):
        """One trim attempt; returns the seconds to wait before the next."""
        try:
            trim(self.path, self.keep_bytes)
        except FileNotFoundError:
            pass
        except OSError as exc:
            if not self.failed:
                print(f"stat_trim: {self.path} is not trimmed, retrying on a backoff: {exc}",
                      file=sys.stderr)
            self.failed = True
            self._delay = min(self._delay * 2, self.max_backoff_s)
            return self._delay
        if self.failed:
            print(f"stat_trim: {self.path} is trimmed again", file=sys.stderr)
        self.failed = False
        self._delay = self.interval_s
        return self._delay

    def start(self):
        """Run on a daemon thread, first attempt after `interval_s`."""
        def loop():
            delay = self.interval_s
            while True:
                time.sleep(delay)
                delay = self.run_once()

        threading.Thread(target=loop, daemon=True, name="stat-trim").start()
        return self


def readable_lines(fh):
    """The lines of a (possibly trimmed) stat CSV opened in binary mode, minus
    any line holding NUL bytes: a punched hole and the two rows it cut, which
    share one line with it."""
    for line in fh:
        if b"\0" not in line:
            yield line


if __name__ == "__main__":
    # `python3 stat_trim.py <file>`: the file's readable lines on stdout.
    with open(sys.argv[1], "rb") as src:
        sys.stdout.buffer.writelines(readable_lines(src))
