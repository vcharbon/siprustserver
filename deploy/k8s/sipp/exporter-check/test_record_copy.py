#!/usr/bin/env python3
"""
Checks of the record copier (../exporter/record_copy.py): the memory it holds
is bounded by its buffer whatever SIPp writes, the lines that do not fit are
counted lost, never written in part, and its status says so once SIPp closed
the FIFO.

  python3 -m unittest discover -s deploy/k8s/sipp/exporter-check
"""
import json
import os
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(HERE), "exporter"))

import record_copy  # noqa: E402

LINE = b'{"call_id":"%05d","ending":"answered"}\n'
PARTIAL = b'{"call_id":"partial"'


class RecordCopyTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.dst = os.path.join(self.dir.name, "records.jsonl")
        self.status = os.path.join(self.dir.name, "records.copy.json")

    def tearDown(self):
        self.dir.cleanup()

    def test_a_stalled_disk_holds_at_most_the_buffer_and_the_lines_past_it_are_counted_lost(self):
        cap = 10 * len(LINE % 0) + len(PARTIAL)
        copy = record_copy.RecordCopy(self.dst, self.status, buffer_bytes=cap)
        for i in range(100):  # the writer is stalled: nothing leaves the buffer
            copy.feed(LINE % i)
            self.assertLessEqual(copy.held(), cap)
        copy.feed(PARTIAL)
        copy.close()
        copy.write_all()
        copy.report_all()
        with open(self.dst, "rb") as fh:
            written = fh.read()
        self.assertEqual(written, b"".join(LINE % i for i in range(10)) + PARTIAL)
        with open(self.status) as fh:
            self.assertEqual(json.load(fh), {"complete": True, "lines": 11, "lost": 90})
        self.assertEqual(copy.held(), 0)

    def test_the_lines_dropped_reach_the_status_while_the_writer_is_stuck(self):
        copy = record_copy.RecordCopy(self.dst, self.status, buffer_bytes=len(LINE % 0))
        threading.Thread(target=copy.report_all, daemon=True).start()
        for i in range(5):  # no writer runs: the first line fills the buffer
            copy.feed(LINE % i)
        deadline = time.monotonic() + 5
        while self.read_status().get("lost") != 4 and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertEqual(self.read_status(), {"complete": False, "lines": 0, "lost": 4})

    def read_status(self):
        try:
            with open(self.status) as fh:
                return json.load(fh)
        except (OSError, ValueError):
            return {}

    def test_the_fifo_reaches_the_record_file_and_completes_when_sipp_closes_it(self):
        fifo = os.path.join(self.dir.name, "records.fifo")
        os.mkfifo(fifo)
        copy = record_copy.start(fifo, self.dst, self.status, buffer_bytes=1 << 20)
        with open(fifo, "wb") as w:  # SIPp: fopen "w", one fflush per record
            for i in range(3):
                w.write(LINE % i)
                w.flush()
        deadline = time.monotonic() + 5
        while not self.read_status().get("complete") and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertEqual(self.read_status(), {"complete": True, "lines": 3, "lost": 0})
        with open(self.dst, "rb") as fh:
            self.assertEqual(fh.read(), b"".join(LINE % i for i in range(3)))


if __name__ == "__main__":
    unittest.main()
