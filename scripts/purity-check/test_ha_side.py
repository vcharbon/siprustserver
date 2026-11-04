"""Checks of the blind/aware boundary and the word net purity.py runs
(scripts/ha_side.py, ADR-0055 §11), on throwaway path-only cargo workspaces."""
import importlib.util
import os
import unittest

from test_purity import IMAGE, SCRIPT, config
from workspace import SCRIPTS, Workspace


class Boundary(unittest.TestCase):
    def workspace(self, crates):
        """A workspace of `(name, side, deps, lib)` crates beside a `bin` the
        configuration names."""
        ws = Workspace()
        self.addCleanup(ws.tmp.cleanup)
        for name, side, deps, lib in crates:
            ws.crate(name, "shell", deps=deps, lib=lib, side=side)
        ws.crate("bin", "shell", bin="fn main() {}\n")
        ws.write("purity.toml", config())
        ws.write("Dockerfile", IMAGE)
        ws.commit()
        return ws

    def test_a_member_declaring_no_side_fails(self):
        ws = self.workspace([("svc", None, (), "")])
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn('svc declares no HA side: [package.metadata.ha] side = "blind" | "aware"',
                      out)

    def test_a_side_other_than_blind_or_aware_fails(self):
        ws = self.workspace([("svc", "both", (), "")])
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("svc declares HA side `both`; a side is `blind` or `aware`", out)

    def test_a_blind_crate_gaining_an_aware_dependency_fails(self):
        ws = self.workspace([("store", "aware", (), ""), ("model", "blind", (), ""),
                             ("svc", "blind", ("model",), ""),
                             ("shell", "aware", ("svc", "store"), "")])
        self.assertEqual(ws.run(SCRIPT), (0, ""))
        ws.crate("svc", "shell", deps=("model", "store"), side="blind")
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("blind crate svc depends on store, which declares side aware", out)

    def test_an_optional_dependency_counts_and_a_dev_dependency_does_not(self):
        ws = self.workspace([("store", "aware", (), ""), ("svc", "blind", (), "")])
        manifest = ws.read("crates/svc/Cargo.toml")
        ws.write("crates/svc/Cargo.toml", manifest + '\n[dev-dependencies]\n'
                 'store = { path = "../store" }\n')
        self.assertEqual(ws.run(SCRIPT), (0, ""))
        ws.write("crates/svc/Cargo.toml", manifest.replace(
            "[dependencies]\n", '[dependencies]\nstore = { path = "../store", optional = true }\n'))
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("blind crate svc depends on store, which declares side aware", out)

    def test_a_blind_crate_naming_a_refused_word_fails_at_its_line(self):
        ws = self.workspace([("svc", "blind", (), "//! One node.\n// the takeover copy\n")])
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn('blind crate svc: crates/svc/src/lib.rs:2 holds "takeover"', out)

    def test_an_untracked_file_of_a_blind_crate_is_read_too(self):
        ws = self.workspace([("svc", "blind", (), "")])
        self.assertEqual(ws.run(SCRIPT), (0, ""))
        ws.write("crates/svc/tests/it.rs", "fn reclaimed_call() {}\n")
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn('crates/svc/tests/it.rs:1 holds "reclaim"', out)

    def test_an_aware_crate_may_name_every_word(self):
        ws = self.workspace([("store", "aware", (), "// takeover, reclaim, replica\n")])
        self.assertEqual(ws.run(SCRIPT), (0, ""))


def ha_side_module():
    """scripts/ha_side.py, imported from the scripts under check."""
    spec = importlib.util.spec_from_file_location("ha_side", os.path.join(SCRIPTS, "ha_side.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class WordNet(unittest.TestCase):
    def setUp(self):
        self.hits = ha_side_module().hits

    def test_every_spelling_of_a_stem_is_refused(self):
        for line, word in [("a Takeover copy", "Takeover"), ("reclaimed", "reclaim"),
                           ("TOMBSTONE", "TOMBSTONE"), ("materialised", "materialis"),
                           ("materialize", "materializ"), ("stand-down", "stand-down"),
                           ("fn stand_down()", "stand_down"), ("StandDown", "StandDown"),
                           ("to stand down", "stand down")]:
            self.assertEqual(self.hits(line), [word], line)

    def test_replica_is_refused_as_a_word_of_its_own_only(self):
        for line in ["the replica", "peek_replica", "ReplicaSet", "KIND-REPLICA(D-1)",
                     "two replicas", "peekReplica"]:
            self.assertEqual(len(self.hits(line)), 1, line)
        for line in ["replicated", "replication", "ReplicatedCall", "nonreplica"]:
            self.assertEqual(self.hits(line), [], line)


if __name__ == "__main__":
    unittest.main()
