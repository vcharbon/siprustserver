#!/usr/bin/env python3
"""
Checks of the SIPp stat exporter (../exporter): it serves the last row of a stat
CSV of any size with flat memory, and the trimmer bounds the disk the CSV holds.

  python3 -m unittest discover -s deploy/k8s/sipp/exporter-check

The large file is sparse (header, a hole, then dense rows): its apparent size is
what a reader that loads the whole file pays for, and it costs no disk or tmpfs.
The served process runs under a 64 MiB memory cgroup when `systemd-run --user`
can make one; its peak RSS is asserted either way.
"""
import csv
import io
import os
import resource
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
EXPORTER_DIR = os.path.join(os.path.dirname(HERE), "exporter")
sys.path.insert(0, EXPORTER_DIR)

import sipp_stat_exporter as exporter  # noqa: E402
import stat_trim  # noqa: E402

MIB = 1024 * 1024
LARGE_APPARENT = 200 * MIB
MEMORY_CAP = 64 * MIB

COLUMNS = (
    ["StartTime", "LastResetTime", "CurrentTime", "CallRate(C)", "TotalCallCreated",
     "CurrentCall", "SuccessfulCall(P)", "SuccessfulCall(C)", "FailedCall(C)",
     "FailedTimeoutOnRecv(C)", "Retransmissions(C)", "ResponseTime1(C)", "CallLength(C)"]
    + [f"Pad{i}(C)" for i in range(40)]
)


def row(n):
    """Data row n: TotalCallCreated=n, SuccessfulCall(C)=n-1, FailedCall(C)=n % 7."""
    cells = ["2026-01-01\t00:00:00.000000\t1767225600.000000"] * 3
    cells += ["12.500", str(n), "40", "3", str(n - 1), str(n % 7), "0", "2",
              "00:00:00:150000", "00:00:35:000000"]
    cells += ["123456789"] * 40
    return (";".join(cells) + ";\n").encode()


def header():
    return (";".join(COLUMNS) + ";\n").encode()


def write_stat(path, first_row, last_row, hole=0):
    """Header, an optional sparse hole of `hole` bytes, then rows first..last."""
    with open(path, "wb") as fh:
        fh.write(header())
        if hole:
            fh.seek(hole, os.SEEK_CUR)
        fh.write(b"".join(row(n) for n in range(first_row, last_row + 1)))


def metric(text, name):
    for line in text.splitlines():
        if line.startswith(name + "{"):
            return line.rsplit(" ", 1)[1]
    return None


def row_metrics(text):
    """The series read off the stat row (everything but the file's disk usage)."""
    return [ln for ln in text.splitlines() if not ln.startswith("sipp_stat_file_allocated_bytes")]


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def memory_cap_prefix():
    """`systemd-run --user --scope` with a 64 MiB cap, or [] where no user manager runs."""
    if not shutil.which("systemd-run"):
        return []
    probe = ["systemd-run", "--user", "--scope", "--quiet",
             "-p", f"MemoryMax={MEMORY_CAP}", "-p", "MemorySwapMax=0", "true"]
    if subprocess.run(probe, capture_output=True).returncode != 0:
        return []
    return probe[:-1]


class TailRead(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="sipp-exporter-check-")
        self.path = os.path.join(self.dir, "stat.csv")

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def test_last_complete_row_is_served(self):
        write_stat(self.path, 1, 5000)
        with open(self.path, "ab") as fh:
            fh.write(row(5001)[:100])  # a row SIPp is still writing
        text = exporter.render(self.path)
        self.assertEqual(metric(text, "sipp_up"), "1")
        self.assertEqual(metric(text, "sipp_calls_created_total"), "5000")
        self.assertEqual(metric(text, "sipp_successful_calls_total"), "4999")
        self.assertEqual(metric(text, "sipp_response_time_ms"), "150")

    def test_header_only_is_down(self):
        with open(self.path, "wb") as fh:
            fh.write(header())
        self.assertEqual(metric(exporter.render(self.path), "sipp_up"), "0")

    def test_missing_file_is_down(self):
        self.assertEqual(metric(exporter.render(self.path), "sipp_up"), "0")

    def test_sparse_file_serves_the_tail(self):
        write_stat(self.path, 900, 1000, hole=8 * MIB)
        text = exporter.render(self.path)
        self.assertEqual(metric(text, "sipp_calls_created_total"), "1000")


class LargeFileUnderMemoryCap(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="sipp-exporter-check-")
        self.path = os.path.join(self.dir, "stat.csv")

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def test_answers_on_a_200_mib_file_under_64_mib(self):
        write_stat(self.path, 1, 2000, hole=LARGE_APPARENT)
        self.assertGreater(os.path.getsize(self.path), LARGE_APPARENT)
        port = free_port()
        env = dict(os.environ, SIPP_STAT_FILE=self.path, EXPORTER_PORT=str(port),
                   SIPP_STAT_KEEP_BYTES="0")
        cmd = memory_cap_prefix() + [sys.executable,
                                     os.path.join(EXPORTER_DIR, "sipp_stat_exporter.py")]
        proc = subprocess.Popen(cmd, env=env, stderr=subprocess.DEVNULL)
        bodies = []
        try:
            deadline = time.monotonic() + 15
            while len(bodies) < 5 and time.monotonic() < deadline:
                if proc.poll() is not None:
                    break
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics",
                                                timeout=5) as resp:
                        bodies.append(resp.read().decode())
                except OSError:
                    time.sleep(0.1)
        finally:
            proc.terminate()
            proc.wait(timeout=10)
        self.assertEqual(len(bodies), 5, f"exporter answered {len(bodies)} of 5 scrapes "
                                         f"(exit {proc.returncode})")
        for body in bodies:
            self.assertEqual(metric(body, "sipp_up"), "1")
            self.assertEqual(metric(body, "sipp_calls_created_total"), "2000")
        peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss * 1024
        self.assertLess(peak, MEMORY_CAP, f"exporter peak RSS {peak / MIB:.1f} MiB")


class Trim(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="sipp-exporter-check-")
        self.path = os.path.join(self.dir, "stat.csv")

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def allocated(self):
        return os.stat(self.path).st_blocks * 512

    def test_trim_keeps_header_and_tail_and_releases_the_rest(self):
        write_stat(self.path, 1, 12000)  # about 5.6 MiB
        size = os.path.getsize(self.path)
        before = row_metrics(exporter.render(self.path))
        try:
            released = stat_trim.trim(self.path, keep_bytes=1 * MIB)
        except OSError as exc:
            self.skipTest(f"no hole punching on {self.dir}: {exc}")
        self.assertGreater(released, 0)
        self.assertEqual(os.path.getsize(self.path), size)
        blk = os.stat(self.path).st_blksize
        self.assertLessEqual(self.allocated(), len(header()) + 1 * MIB + 2 * blk)
        with open(self.path, "rb") as fh:
            self.assertEqual(fh.readline(), header())
        self.assertEqual(row_metrics(exporter.render(self.path)), before)

    def test_under_the_bound_nothing_is_released(self):
        write_stat(self.path, 1, 1000)  # under 2 x keep
        before = self.allocated()
        self.assertEqual(stat_trim.trim(self.path, keep_bytes=1 * MIB), 0)
        self.assertEqual(self.allocated(), before)

    def test_writer_appends_after_a_trim(self):
        write_stat(self.path, 1, 12000)
        with open(self.path, "r+b") as writer:  # SIPp's ofstream: its own offset
            writer.seek(0, os.SEEK_END)
            try:
                stat_trim.trim(self.path, keep_bytes=1 * MIB)
            except OSError as exc:
                self.skipTest(f"no hole punching on {self.dir}: {exc}")
            writer.write(row(12001))
        self.assertEqual(metric(exporter.render(self.path), "sipp_calls_created_total"),
                         "12001")


class ReadableLines(unittest.TestCase):
    """A trimmed file read back as CSV: the rows on each side of the hole are cut
    mid-row and share one line with the hole; that line is dropped, never spliced."""

    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="sipp-exporter-check-")
        self.path = os.path.join(self.dir, "stat.csv")

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def test_every_line_of_a_trimmed_file_is_a_whole_row(self):
        write_stat(self.path, 1, 12000)
        try:
            stat_trim.trim(self.path, keep_bytes=1 * MIB)
        except OSError as exc:
            self.skipTest(f"no hole punching on {self.dir}: {exc}")
        with open(self.path, "rb") as fh:
            text = b"".join(stat_trim.readable_lines(fh)).decode()
        rows = list(csv.DictReader(io.StringIO(text), delimiter=";"))
        self.assertGreater(len(rows), 1000)
        created = []
        for r in rows:
            self.assertNotIn(None, r)
            self.assertNotIn(None, r.values())
            float(r["CurrentTime"].split("\t")[-1])
            created.append(int(r["TotalCallCreated"]))
        self.assertEqual(created[-1], 12000)
        self.assertEqual(created, sorted(set(created)))


class TrimFailure(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="sipp-exporter-check-")
        self.path = os.path.join(self.dir, "stat.csv")
        write_stat(self.path, 1, 100)

    def tearDown(self):
        os.chmod(self.path, 0o644)
        shutil.rmtree(self.dir, ignore_errors=True)

    @unittest.skipIf(os.geteuid() == 0, "root writes a read-only file")
    def test_a_failed_trim_is_retried_on_a_backoff_and_exported(self):
        trimmer = stat_trim.Trimmer(self.path, keep_bytes=1 * MIB, interval_s=60,
                                    max_backoff_s=200)
        os.chmod(self.path, 0o444)
        self.assertEqual(trimmer.run_once(), 120)
        self.assertEqual(trimmer.run_once(), 200)
        self.assertEqual(metric(exporter.render(self.path, trimmer=trimmer),
                                "sipp_stat_trim_failed"), "1")
        os.chmod(self.path, 0o644)
        self.assertEqual(trimmer.run_once(), 60)
        self.assertEqual(metric(exporter.render(self.path, trimmer=trimmer),
                                "sipp_stat_trim_failed"), "0")

    def test_allocated_bytes_are_exported(self):
        v = metric(exporter.render(self.path), "sipp_stat_file_allocated_bytes")
        self.assertEqual(int(v), os.stat(self.path).st_blocks * 512)


if __name__ == "__main__":
    unittest.main()
