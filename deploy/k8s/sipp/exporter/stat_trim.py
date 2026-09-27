"""
Bounds the disk a SIPp stat CSV holds while SIPp keeps appending to it.

SIPp has no rotation for `-trace_stat`: it writes the file through one stream at
its own offset for the whole run. The trimmer punches a hole
(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) between the header line and the last
`keep_bytes`: the apparent size and the writer's offset are unchanged, the
blocks in between are released and read back as NUL bytes. The header and the
recent rows stay readable, so the exporter's tail read is unaffected.

Linux only; on a filesystem without hole punching `trim` raises OSError and the
background loop stops after one line on stderr.
"""
import ctypes
import ctypes.util
import errno
import os
import sys
import threading

FALLOC_FL_KEEP_SIZE = 0x01
FALLOC_FL_PUNCH_HOLE = 0x02

# Seconds between two trims; a stat row a second grows the file by ~40 MB a day.
INTERVAL_S = 60
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


def _loop(path, keep_bytes, stop):
    while not stop.wait(INTERVAL_S):
        try:
            trim(path, keep_bytes)
        except FileNotFoundError:
            continue
        except OSError as exc:
            print(f"stat_trim: {path} is not trimmed: {exc}", file=sys.stderr)
            return


def start(path, keep_bytes):
    """Trim `path` every INTERVAL_S on a daemon thread; returns its stop event."""
    stop = threading.Event()
    threading.Thread(target=_loop, args=(path, keep_bytes, stop), daemon=True,
                     name="stat-trim").start()
    return stop
