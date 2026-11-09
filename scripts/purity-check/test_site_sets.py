"""Checks of the loop and recursion site sets of scripts/panic-sites.py
(ADR-0057 §5) on throwaway cargo workspaces, and the scanner's own tests.

The scanner's lines are recorded (`--json`), except in `RealScan`, which runs
tooling/site-scan and clippy once on a small workspace.
"""
import json
import os
import subprocess
import tempfile
import unittest

from test_panic_sites import HEADER, LIB, REAL_CLIPPY_TOML, SCRIPT, ZERO, ZERO_PURE, Case
from workspace import SCRIPTS, Workspace

BASE_LINTS = ('DENIED = ()\nPURE_DENIED = ()\n'
              'LISTS = ("disallowed_macros", "disallowed_methods", "disallowed_types")\n')
SETS_LINTS = BASE_LINTS + 'SITE_SETS = ("loop", "recursion")\nSITE_RULES = 1\n'
OLD_RULES_LINTS = BASE_LINTS + 'SITE_SETS = ("loop", "recursion")\nSITE_RULES = 0\n'


def site(crate, set_, file, line, message="`while`"):
    """One line of tooling/site-scan's output."""
    return json.dumps({"reason": "site-scan", "crate": crate, "set": set_, "file_name": file,
                       "line_start": line, "line_end": line, "column_start": 5,
                       "column_end": 10, "message": message})


class Sets(Case):
    def test_an_unlisted_loop_site_fails_with_its_class_and_hint(self):
        ws = self.workspace(crates=(("a", "pure"),))
        ws.write("crates/a/src/lib.rs", ZERO_PURE + LIB)
        ws.commit()
        rc, out = self.check(ws, [site("a", "loop", "crates/a/src/lib.rs", 3)])
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/lib.rs has 1 sites (1 loop), budget 0 (unlisted)", out)
        self.assertIn("crates/a/src/lib.rs:3 site::loop", out)
        self.assertIn("`bounds::bounded`", out)
        self.assertNotIn("a panic-class site is rewritten", out)

    def test_a_listed_site_passes_and_leaves_its_crate_at_zero(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n", crates=(("a", "pure"),))
        ws.write("crates/a/src/lib.rs", ZERO_PURE + LIB)
        ws.commit()
        sites = [site("a", "loop", "crates/a/src/lib.rs", 3)]
        rc, out = self.check(ws, sites)
        self.assertEqual(rc, 0, out)
        ws.write("crates/a/src/lib.rs", LIB)
        ws.commit()
        rc, out = self.check(ws, sites)
        self.assertEqual(rc, 1)
        self.assertIn("a is at zero: crates/a/src/lib.rs carries", out)

    def test_a_shell_crate_counts_its_recursions_and_not_its_loops(self):
        ws = self.workspace(crates=(("a", "shell"),))
        ws.write("crates/a/src/lib.rs", ZERO + LIB)
        ws.commit()
        rc, out = self.check(ws, [
            site("a", "loop", "crates/a/src/lib.rs", 3),
            site("a", "recursion", "crates/a/src/lib.rs", 7, "recursion with no depth guard: f")])
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/lib.rs has 1 sites (1 recursion), budget 0 (unlisted)", out)
        self.assertNotIn("site::loop", out)

    def test_a_service_built_on_the_sdk_counts_its_loops_and_the_b2bua_does_not(self):
        ws = self.workspace(crates=(("b2bua-sdk", "pure"),))
        ws.write("crates/b2bua-sdk/src/lib.rs", ZERO_PURE + LIB)
        ws.crate("b2bua-core", "pure", deps=("b2bua-sdk",), lib=ZERO_PURE + LIB)
        ws.crate("svc", "shell", deps=("b2bua-sdk",), lib=ZERO + LIB)
        ws.crate("b2bua", "shell", deps=("b2bua-sdk", "b2bua-core"), lib=ZERO + LIB)
        ws.commit()
        rc, out = self.check(ws, [site("svc", "loop", "crates/svc/src/lib.rs", 3),
                                  site("b2bua", "loop", "crates/b2bua/src/lib.rs", 3)])
        self.assertEqual(rc, 1)
        self.assertIn("crates/svc/src/lib.rs has 1 sites (1 loop)", out)
        self.assertNotIn("crates/b2bua/src/lib.rs", out)

    def test_a_record_with_no_complete_scan_is_refused(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n", crates=(("a", "pure"),))
        ws.write("crates/a/src/lib.rs", ZERO_PURE + LIB)
        ws.write("pass.json", site("a", "loop", "crates/a/src/lib.rs", 3) + "\n")
        ws.commit()
        rc, out = ws.run(SCRIPT, "--update", "--json", os.path.join(ws.root, "pass.json"))
        self.assertEqual(rc, 1)
        self.assertIn("holds no complete site scan", out)
        self.assertEqual(ws.read("panic-sites.allow"), HEADER + "crates/a/src/lib.rs 1\n")

    def test_the_bounds_crate_is_outside_the_loop_set(self):
        ws = self.workspace(crates=(("bounds", "pure"),))
        ws.write("crates/bounds/src/lib.rs", ZERO_PURE + LIB)
        ws.commit()
        rc, out = self.check(ws, [site("bounds", "loop", "crates/bounds/src/lib.rs", 3)])
        self.assertEqual(rc, 0, out)


class Revealed(Case):
    def sites(self, lints):
        ws = self.workspace(crates=(("a", "pure"),))
        ws.write("crates/a/src/lib.rs", ZERO_PURE + LIB)
        ws.write("scripts/panic_sites_lints.py", lints)
        ws.commit()
        return ws, [site("a", "loop", "crates/a/src/lib.rs", 3),
                    site("a", "recursion", "crates/a/src/lib.rs", 9, "recursion: f")]

    def test_a_base_without_the_site_sets_reveals_their_sites(self):
        ws, sites = self.sites(BASE_LINTS)
        rc, out = self.check(ws, sites, "--update")
        self.assertEqual(rc, 0, out)
        self.assertEqual(ws.read("panic-sites.allow"), HEADER + "crates/a/src/lib.rs 2\n")
        rc, out = self.check(ws, sites)
        self.assertEqual(rc, 0, out)

    def test_a_base_on_older_scanner_rules_reveals_their_sites(self):
        ws, sites = self.sites(OLD_RULES_LINTS)
        rc, out = self.check(ws, sites, "--update")
        self.assertEqual(rc, 0, out)
        self.assertEqual(ws.read("panic-sites.allow"), HEADER + "crates/a/src/lib.rs 2\n")

    def test_a_base_counting_the_site_sets_lets_no_new_site_in(self):
        ws, sites = self.sites(SETS_LINTS)
        rc, out = self.check(ws, sites, "--update")
        self.assertEqual(rc, 1)
        self.assertEqual(ws.read("panic-sites.allow"), HEADER)


class RealScan(Case):
    LOOPS = "pub fn drain(mut v: Vec<u8>) {\n    while v.pop().is_some() {}\n}\n"
    CYCLES = (
        "pub fn walk(n: u8) -> u8 {\n    if n == 0 { 0 } else { walk(n - 1) }\n}\n"
        "pub fn spin(mut v: Vec<u8>) {\n    loop {\n        if v.pop().is_none() { break; }\n"
        "    }\n}\n"
        "pub fn guarded(d: &bounds::Depth) -> Result<(), ()> {\n"
        "    let deeper = d.descend()?;\n    guarded(&deeper)\n}\n")
    BOUNDS = ("pub struct Depth;\nimpl Depth {\n"
              "    pub fn descend(&self) -> Result<Depth, ()> {\n        Err(())\n    }\n}\n")

    def test_the_scanner_counts_beside_clippy_and_records_its_lines(self):
        ws = Workspace()
        self.addCleanup(ws.tmp.cleanup)
        ws.crate("a", "pure", lib=self.LOOPS)
        ws.crate("bounds", "pure", lib=self.BOUNDS)
        ws.crate("b", "shell", deps=("bounds",), lib=self.CYCLES)
        with open(REAL_CLIPPY_TOML) as f:
            ws.write("clippy.toml", f.read())
        ws.write("panic-sites.allow", HEADER)
        ws.commit()
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        record = os.path.join(tmp.name, "pass.json")
        rc, out = ws.run(SCRIPT, "--report", "--record", record)
        self.assertEqual(rc, 0, out)
        self.assertIn("crates/a/src/lib.rs 1\n", out)
        self.assertIn("crates/b/src/lib.rs 1\n", out)
        self.assertIn("# bounds (pure): at zero", out)
        rc, again = ws.run(SCRIPT, "--report", "--json", record)
        self.assertEqual(rc, 0, again)
        self.assertEqual(again, out)
        rc, out = ws.run(SCRIPT, "--json", record)
        self.assertIn("crates/b/src/lib.rs:1 site::recursion", out)


class Scanner(unittest.TestCase):
    def test_the_scanner_s_own_tests_pass(self):
        manifest = os.path.join(SCRIPTS, "..", "tooling", "site-scan", "Cargo.toml")
        out = subprocess.run(["cargo", "test", "--quiet", "--manifest-path", manifest],
                             capture_output=True, text=True)
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)


if __name__ == "__main__":
    unittest.main()
