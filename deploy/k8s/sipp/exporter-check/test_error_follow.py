#!/usr/bin/env python3
"""
Checks of the SIPp error-file follower (../exporter/error_follow.py): its counters
cover the whole run while SIPp rotates the file under it (-ringbuffer_*).

  python3 -m unittest discover -s deploy/k8s/sipp/exporter-check

The last check runs a real SIPp with the lanes' rotation flags when `sipp` is on
PATH, and is skipped otherwise.
"""
import os
import re
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

# The HA tooling's base UAC, the scenario every SIPp stream of a run plays.
UAC = os.path.join(SIPP_DIR, "..", "..", "..", "tooling", "hakit", "content", "scenarios",
                   "uac-basic-hold.xml")
KEY_DEFAULT = re.compile(r"^\s*hakit-key:\s*([a-z_][a-z0-9_]*)=(\S*)\s*$", re.MULTILINE)


def uac_keys(**keys):
    """The `-key` arguments of UAC: the defaults its header declares, then `keys`."""
    with open(UAC, encoding="latin-1") as fh:
        header = fh.read().split("<scenario", 1)[0]
    merged = {**dict(KEY_DEFAULT.findall(header)), **keys}
    return [a for k, v in sorted(merged.items()) for a in ("-key", k, v)]

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


def error_entry(call_id):
    """An entry that is an error: a send's retransmissions ran out."""
    return f"{STAMP}Call-Id: {call_id}, timeout on max UDP retrans for message 9, jumping to label 20 \n"


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
        self.append(error_entry("c2"))
        self.rotate(1)
        self.follower.poll()
        self.append(error_entry("c3"))
        self.rotate(2)
        self.follower.poll()
        self.append(error_entry("c4"))
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
        self.append(error_entry("c1") * 3)
        self.follower.poll()
        open(self.path, "w").close()
        self.append(error_entry("c2"))
        self.follower.poll()
        self.assertEqual(self.follower.snapshot().entries, 4)

    def test_the_keep_file_holds_every_entry_but_the_scenarios_own_timer(self):
        keep = os.path.join(self.dir, "errors.kept.log")
        follower = error_follow.ErrorFollower(self.path, keep_path=keep)
        retrans = f"{STAMP}Call-Id: c4, timeout on max UDP retrans for message 9, jumping to label 20 \n"
        abort = abort_entry("c2", "OPTIONS", "SIP/2.0 480 Temporarily Unavailable")
        self.append(timeout_entry("c1") + abort + timeout_entry("c3"))
        follower.poll()
        self.append(retrans[:30])
        follower.poll()
        self.append(retrans[30:] + timeout_entry("c5"))
        follower.poll()
        follower.close()
        self.assertEqual((follower.snapshot().entries, follower.snapshot().timer_jumps), (2, 3))
        with open(keep) as fh:
            self.assertEqual(fh.read(), abort + retrans)

    def test_a_scenarios_own_timer_is_counted_apart_from_the_errors(self):
        """A recv timeout that jumps to its label is a scenario's tick, not an
        error: entries counts the errors alone, timer_jumps the ticks."""
        retrans = f"{STAMP}Call-Id: c4, timeout on max UDP retrans for message 9, jumping to label 20 \n"
        self.append(timeout_entry("c1") * 3 + abort_entry("c2", "OPTIONS", "SIP/2.0 480 Temporarily Unavailable") + retrans)
        self.follower.poll()
        counts = self.follower.snapshot()
        self.assertEqual((counts.entries, counts.timer_jumps), (2, 3))

    def test_a_missing_file_counts_nothing_until_it_appears(self):
        self.follower.poll()
        self.assertEqual(self.follower.snapshot().entries, 0)
        self.append(error_entry("c1"))
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
                fh.write("SEQUENTIAL\nring;X-Test: ring;;;\n")
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
                     "-sf", UAC, "-inf", "t.csv",
                     *uac_keys(run="ring", stream="ring", shape="basic", setup_ms="100", hold_ms="100"),
                     "-i", "127.0.0.1", "-p", str(socket_port()), "-m", "40", "-r", "20",
                     "-recv_timeout", "100", "-max_invite_retrans", "1", "-nostdin", "-trace_err",
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


class TrimmedErrorFileTest(unittest.TestCase):
    """The error file of a SIPp run without rotation, bounded by stat_trim: the
    follower keeps counting what SIPp appends after a trim."""

    def test_the_follower_counts_through_a_trim(self):
        import stat_trim
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "errors.log")
            entry = error_entry("c1")
            with open(path, "w") as fh:
                fh.write("The following events occurred:\n")
                fh.write(entry * (3 * 1024 * 1024 // len(entry)))
            follower = error_follow.ErrorFollower(path)
            follower.poll()
            before = follower.snapshot().entries
            stat_trim.trim(path, 256 * 1024)
            self.assertLess(os.stat(path).st_blocks * 512, 1024 * 1024)
            with open(path, "a") as fh:
                fh.write(entry * 10)
            follower.poll()
            self.assertEqual(follower.snapshot().entries, before + 10)
