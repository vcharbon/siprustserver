#!/usr/bin/env python3
"""
Checks of the call-outcome record shipper (../exporter/record_ship.py): every
line the UAC writes ships once, verbatim in `_msg`, a failed post loses
nothing, and a partial line waits for its end.

  python3 -m unittest discover -s deploy/k8s/sipp/exporter-check
"""
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(HERE), "exporter"))

import record_ship  # noqa: E402
import sipp_stat_exporter  # noqa: E402


def record(call_id, ending="answered"):
    return json.dumps({"schema": "call-outcome.v1", "run": "r1", "source": "sipp", "stream": "basic",
                       "shape": "basic", "call_id": call_id, "ended_at_ms": 1767261600123.5,
                       "ending": ending})


class Sink:
    def __init__(self):
        self.up = True
        self.posts = []

    def __call__(self, url, body):
        if not self.up:
            return False
        self.posts.append((url, body.decode()))
        return True

    def lines(self):
        return [json.loads(ln) for _, body in self.posts for ln in body.splitlines() if ln]


class RecordShipTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "records.jsonl")
        self.sink = Sink()
        self.shipper = record_ship.RecordShipper(self.path, "http://vl:9428/", "r1", "basic", "basic", post=self.sink)

    def tearDown(self):
        self.dir.cleanup()

    def append(self, text):
        with open(self.path, "a") as fh:
            fh.write(text)

    def test_each_record_ships_once_verbatim(self):
        self.append(record("a") + "\n" + record("b", "rejected") + "\n")
        self.shipper.poll()
        self.shipper.poll()
        lines = self.sink.lines()
        self.assertEqual([ln["call_id"] for ln in lines], ["a", "b"])
        self.assertEqual(json.loads(lines[0]["_msg"])["call_id"], "a")
        self.assertEqual(lines[0]["_time"], "2026-01-01T10:00:00.123Z")
        self.assertEqual((lines[0]["run"], lines[0]["source"], lines[0]["stream"]), ("r1", "sipp", "basic"))
        self.assertIn("_stream_fields=run,source,stream", self.sink.posts[0][0])
        snap = self.shipper.snapshot()
        self.assertEqual(snap.by_ending, {"answered": 1, "rejected": 1})
        self.assertEqual((snap.shipped, snap.pending), (2, 0))

    def test_a_failed_post_keeps_the_lines(self):
        self.append(record("a") + "\n")
        self.sink.up = False
        self.shipper.poll()
        self.assertEqual(self.shipper.snapshot().pending, 1)
        self.sink.up = True
        self.append(record("b") + "\n")
        self.shipper.poll()
        self.assertEqual([ln["call_id"] for ln in self.sink.lines()], ["a", "b"])
        snap = self.shipper.snapshot()
        self.assertEqual((snap.shipped, snap.failed_posts, snap.pending), (2, 1, 0))

    def test_a_partial_line_waits_for_its_end(self):
        line = record("a")
        self.append(line[:20])
        self.shipper.poll()
        self.assertEqual(self.sink.lines(), [])
        self.append(line[20:] + "\n")
        self.shipper.poll()
        self.assertEqual([ln["call_id"] for ln in self.sink.lines()], ["a"])

    def test_an_unparsed_line_still_ships(self):
        self.append("not json\n")
        self.shipper.poll()
        (ln,) = self.sink.lines()
        self.assertEqual((ln["_msg"], ln["unparsed"], ln["run"]), ("not json", "true", "r1"))
        self.assertEqual(self.shipper.snapshot().by_ending, {"unparsed": 1})

    def test_the_exporter_renders_the_counters(self):
        self.append(record("a") + "\n")
        self.shipper.poll()
        text = sipp_stat_exporter.render(os.path.join(self.dir.name, "absent.csv"), shipper=self.shipper)
        self.assertIn('sipp_records_total{scenario="unknown",role="uac",ending="answered"} 1', text)
        self.assertIn("sipp_records_shipped_total", text)


if __name__ == "__main__":
    unittest.main()
