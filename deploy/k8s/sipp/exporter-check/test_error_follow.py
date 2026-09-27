#!/usr/bin/env python3
"""
Checks of the SIPp error-file follower (../exporter/error_follow.py): its counters
cover the whole run while SIPp rotates the file under it (-ringbuffer_*).

  python3 -m unittest discover -s deploy/k8s/sipp/exporter-check

The last check runs a real SIPp with the lanes' rotation flags when `sipp` is on
PATH, and is skipped otherwise.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SIPP_DIR = os.path.dirname(HERE)
sys.path.insert(0, os.path.join(SIPP_DIR, "exporter"))

import error_follow  # noqa: E402

STAMP = "2026-01-01\t10:00:00.000000\t1767261600.000000: "


def abort_entry(call_id, expecting, received):
    """A SIPp unexpected-message entry: the received message follows on its lines."""
    return (f"{STAMP}Aborting call on unexpected message for Call-Id '{call_id}': "
            f"while expecting '{expecting}' (index 12), received '{received}\n"
            f"Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\n"
            f"Call-ID: {call_id}\n"
            f"Content-Length: 0\n\n'.\n")


def timeout_entry(call_id):
    return (f"{STAMP}Call-Id: {call_id}, receive timeout on message OPTIONS:12, "
            f"jumping to label 14\n")


POST_ANSWER_480 = (("action", "aborted"), ("expecting", "OPTIONS"), ("received", "480"))


class Follow(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="sipp-error-follow-")
        self.path = os.path.join(self.dir, "errors.log")
        self.follower = error_follow.ErrorFollower(self.path)

    def tearDown(self):
        self.follower.close()
        shutil.rmtree(self.dir, ignore_errors=True)

    def append(self, text, path=None):
        with open(path or self.path, "a") as fh:
            fh.write(text)

    def rotate(self, n):
        """SIPp's ring buffer: rename the file away, keep one rotated file, reopen."""
        rotated = os.path.join(self.dir, f"scn_1_errors_{n}.log")
        for old in os.listdir(self.dir):
            if old.startswith("scn_1_errors_"):
                os.unlink(os.path.join(self.dir, old))
        os.rename(self.path, rotated)
        open(self.path, "w").close()
        return rotated

    def test_an_abort_survives_two_rotations(self):
        self.append("The following events occurred:\n" + abort_entry("c1", "OPTIONS", "SIP/2.0 480 Temporarily Unavailable"))
        self.follower.poll()
        self.append(timeout_entry("c2"))
        self.rotate(1)
        self.follower.poll()
        self.append(timeout_entry("c3"))
        self.rotate(2)
        self.follower.poll()
        self.append(timeout_entry("c4"))
        self.follower.poll()
        counts = self.follower.snapshot()
        self.assertEqual(counts.unexpected, {POST_ANSWER_480: 1})
        self.assertEqual(counts.entries, 4)

    def test_lines_written_just_before_a_rotation_are_read(self):
        self.append(timeout_entry("c1"))
        self.follower.poll()
        rotated = os.path.join(self.dir, "scn_1_errors_1.log")
        self.append(abort_entry("c2", "OPTIONS", "SIP/2.0 480 Temporarily Unavailable"))
        os.rename(self.path, rotated)
        open(self.path, "w").close()
        os.unlink(rotated)  # the next rotation removed it before a poll ran
        self.follower.poll()
        self.assertEqual(self.follower.snapshot().unexpected, {POST_ANSWER_480: 1})

    def test_labels_name_expected_step_and_received_message(self):
        self.append(abort_entry("c1", "200", "SIP/2.0 480 Temporarily Unavailable"))
        self.append(abort_entry("c2", "OPTIONS", "BYE sip:a@10.0.0.2 SIP/2.0"))
        self.append(f"{STAMP}Continuing call on unexpected message for Call-Id 'c3': "
                    f"while sending (index 4), received 'SIP/2.0 503 Service Unavailable\n'.\n")
        self.follower.poll()
        self.assertEqual(self.follower.snapshot().unexpected, {
            (("action", "aborted"), ("expecting", "200"), ("received", "480")): 1,
            (("action", "aborted"), ("expecting", "OPTIONS"), ("received", "BYE")): 1,
            (("action", "continued"), ("expecting", "sending"), ("received", "503")): 1,
        })

    def test_a_line_written_in_two_parts_counts_once(self):
        entry = abort_entry("c1", "OPTIONS", "SIP/2.0 480 Temporarily Unavailable")
        self.append(entry[:40])
        self.follower.poll()
        self.append(entry[40:])
        self.follower.poll()
        self.assertEqual(self.follower.snapshot().unexpected, {POST_ANSWER_480: 1})

    def test_truncated_in_place_reads_from_the_start(self):
        self.append(timeout_entry("c1") * 3)
        self.follower.poll()
        open(self.path, "w").close()
        self.append(timeout_entry("c2"))
        self.follower.poll()
        self.assertEqual(self.follower.snapshot().entries, 4)

    def test_a_missing_file_counts_nothing_until_it_appears(self):
        self.follower.poll()
        self.assertEqual(self.follower.snapshot().entries, 0)
        self.append(timeout_entry("c1"))
        self.follower.poll()
        self.assertEqual(self.follower.snapshot().entries, 1)


@unittest.skipUnless(shutil.which("sipp"), "no sipp on PATH")
class SippRingBuffer(unittest.TestCase):
    """SIPp with the lanes' flags (-ringbuffer_files 1) keeps one rotated file, and
    the follower counts the entries of the files the ring already removed."""

    def test_ring_keeps_one_rotated_file_and_the_follower_counts_them_all(self):
        work = tempfile.mkdtemp(prefix="sipp-ring-")
        try:
            with open(os.path.join(work, "t.csv"), "w") as fh:
                fh.write("SEQUENTIAL\n127.0.0.1\n")
            dead = socket_port()
            follower = error_follow.ErrorFollower(os.path.join(work, "errors.log"))
            stop = threading.Event()

            def follow():
                while not stop.is_set():
                    follower.poll()
                    time.sleep(0.01)

            t = threading.Thread(target=follow)
            t.start()
            try:
                subprocess.run(
                    ["sipp", f"127.0.0.1:{dead}",
                     "-sf", os.path.join(SIPP_DIR, "scenarios", "uac-basic.xml"),
                     "-inf", "t.csv", "-key", "xapi", "{}", "-s", "svc",
                     "-i", "127.0.0.1", "-p", str(socket_port()), "-m", "40", "-r", "20",
                     "-recv_timeout", "100", "-nostdin", "-trace_err",
                     "-error_file", os.path.join(work, "errors.log"),
                     "-ringbuffer_files", "1", "-ringbuffer_size", "1000"],
                    cwd=work, capture_output=True, timeout=60)
            finally:
                stop.set()
                t.join()
            follower.poll()
            follower.close()
            rotated = [f for f in os.listdir(work) if "_errors_" in f]
            self.assertEqual(len(rotated), 1, os.listdir(work))
            on_disk = 0
            for name in rotated + ["errors.log"]:
                with open(os.path.join(work, name), "rb") as fh:
                    on_disk += sum(1 for ln in fh if error_follow.ENTRY.match(ln))
            self.assertGreater(follower.snapshot().entries, on_disk)
        finally:
            shutil.rmtree(work, ignore_errors=True)


def socket_port():
    import socket
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


if __name__ == "__main__":
    unittest.main()
