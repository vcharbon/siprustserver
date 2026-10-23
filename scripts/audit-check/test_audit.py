"""Checks of scripts/audit.py on throwaway git repositories.

A stub `cargo` first on PATH answers `cargo audit` with a canned report and
records every call; one check runs the real cargo-audit against the local
advisory database on a lockfile holding a known-vulnerable crate (skipped when
cargo-audit or the database is missing).
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "..", "audit.py")

STUB = r'''#!/usr/bin/env python3
import json, os, sys, time
args = sys.argv[1:]
with open(os.environ["STUB_LOG"], "a") as f:
    f.write(" ".join(args) + "\n")
if args[:1] == ["audit"]:
    if "--no-fetch" not in args and os.environ.get("STUB_FETCH_SLEEPS"):
        time.sleep(float(os.environ["STUB_FETCH_SLEEPS"]))
    if "--no-fetch" not in args and os.environ.get("STUB_FETCH_FAILS"):
        print(os.environ["STUB_FETCH_FAILS"], file=sys.stderr)
        sys.exit(1)
    with open(os.environ["STUB_REPORT"]) as f:
        report = json.load(f)
    print(json.dumps(report) if "--json" in args else "terminal report")
    sys.exit(1 if report["vulnerabilities"]["list"] else 0)
print("outdated: " + " ".join(args))
'''

EMPTY_LOCK = "version = 4\n"
VULNERABLE_LOCK = """version = 4

[[package]]
name = "quick-xml"
version = "0.26.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "7f50b1c63b38611e7d4d7f68b82d3ad0cc71a2ad2e7f61fc10f1328d917c93cd"
"""


def report(vulns=(), unmaintained=(), notice=()):
    def warnings(kind, rows):
        return [{"kind": kind, "advisory": {"id": i},
                 "package": {"name": name, "version": "0.1.0"}} for name, i in rows]
    return {
        "vulnerabilities": {"found": bool(vulns), "count": len(vulns), "list": [
            {"advisory": {"id": i, "title": f"{name} is broken"},
             "versions": {"patched": [">=9.0.0"]},
             "package": {"name": name, "version": "1.0.0"}} for name, i in vulns]},
        "warnings": {"unmaintained": warnings("unmaintained", unmaintained),
                     "notice": warnings("notice", notice)},
    }


class Repo:
    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = os.path.join(self.tmp.name, "repo")
        self.stubs = os.path.join(self.tmp.name, "bin")
        self.db = os.path.join(self.tmp.name, "db")
        self.log = os.path.join(self.tmp.name, "calls.log")
        self.report = os.path.join(self.tmp.name, "report.json")
        os.makedirs(self.root)
        os.makedirs(self.stubs)
        os.makedirs(os.path.join(self.db, ".git"))
        for name, text in (("cargo", STUB), ("cargo-audit", "#!/bin/sh\n"),
                           ("cargo-outdated", "#!/bin/sh\n"), ("cargo-upgrade", "#!/bin/sh\n")):
            path = os.path.join(self.stubs, name)
            with open(path, "w") as f:
                f.write(text)
            os.chmod(path, 0o755)
        self.fetched(hours_ago=1)
        self.answer(report())
        self.git("init", "-q", "-b", "develop")
        # No detached auto-maintenance writes into .git while the directory is removed.
        self.git("config", "maintenance.auto", "false")
        self.git("config", "gc.auto", "0")
        self.write("Cargo.lock", EMPTY_LOCK)
        self.commit()

    def git(self, *args):
        subprocess.run(["git", "-C", self.root, *args], check=True, capture_output=True)

    def write(self, rel, text):
        path = os.path.join(self.root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)

    def commit(self):
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "c")

    def fetched(self, hours_ago):
        stamp = os.path.join(self.db, ".git", "FETCH_HEAD")
        with open(stamp, "w") as f:
            f.write("")
        when = time.time() - hours_ago * 3600
        os.utime(stamp, (when, when))

    def answer(self, rep):
        with open(self.report, "w") as f:
            json.dump(rep, f)

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return f.read().splitlines()

    def run(self, *flags, env=None, stubbed=True):
        e = dict(os.environ, STUB_LOG=self.log, STUB_REPORT=self.report, **(env or {}))
        if stubbed:
            e["PATH"] = self.stubs + os.pathsep + e["PATH"]
        r = subprocess.run([sys.executable, SCRIPT, "-C", self.root, *flags],
                           capture_output=True, text=True, env=e)
        return r.returncode, r.stdout + r.stderr


class Case(unittest.TestCase):
    def repo(self):
        r = Repo()
        self.addCleanup(r.tmp.cleanup)
        return r


class Gate(Case):
    def test_a_vulnerability_on_an_untouched_lockfile_is_a_warning(self):
        r = self.repo()
        r.answer(report(vulns=[("rustls", "RUSTSEC-2026-0001")]))
        rc, out = r.run("--db", r.db)
        self.assertEqual(rc, 0, out)
        self.assertIn("audit: warning: 1 vulnerability", out)
        self.assertIn("so they do not fail", out)
        self.assertIn("vulnerable rustls 1.0.0 RUSTSEC-2026-0001", out)

    def test_a_vulnerability_fails_when_the_branch_touches_the_lockfile(self):
        r = self.repo()
        r.write("Cargo.lock", EMPTY_LOCK + "# changed\n")
        r.answer(report(vulns=[("rustls", "RUSTSEC-2026-0001")]))
        rc, out = r.run("--db", r.db)
        self.assertEqual(rc, 1, out)
        self.assertIn("audit: FAIL: 1 vulnerability", out)
        self.assertIn("the branch touches Cargo.lock", out)
        self.assertIn("RUSTSEC-2026-0001", out)

    def test_a_new_audit_configuration_gates_too(self):
        r = self.repo()
        r.write(".cargo/audit.toml", "[advisories]\nignore = []\n")
        r.answer(report(vulns=[("rustls", "RUSTSEC-2026-0001")]))
        rc, out = r.run("--db", r.db)
        self.assertEqual(rc, 1, out)
        self.assertIn("the branch touches .cargo/audit.toml", out)

    def test_a_committed_lockfile_change_is_held_to_the_base(self):
        r = self.repo()
        r.git("checkout", "-q", "-b", "topic")
        r.write("Cargo.lock", EMPTY_LOCK + "# changed\n")
        r.commit()
        r.answer(report(vulns=[("rustls", "RUSTSEC-2026-0001")]))
        self.assertEqual(r.run("--db", r.db)[0], 1)
        self.assertEqual(r.run("--db", r.db, "--base", "HEAD")[0], 0)

    def test_without_a_local_develop_the_base_is_origin_develop(self):
        r = self.repo()
        r.git("update-ref", "refs/remotes/origin/develop", "HEAD")
        r.git("branch", "-m", "develop", "topic")
        r.write("Cargo.lock", EMPTY_LOCK + "# changed\n")
        r.commit()
        r.answer(report(vulns=[("rustls", "RUSTSEC-2026-0001")]))
        rc, out = r.run("--db", r.db)
        self.assertEqual(rc, 1, out)
        self.assertIn("the branch touches Cargo.lock", out)

    def test_a_notice_is_counted_and_listed(self):
        r = self.repo()
        r.answer(report(notice=[("old-crate", "RUSTSEC-2026-0002")]))
        rc, out = r.run("--db", r.db)
        self.assertEqual(rc, 0, out)
        self.assertIn("audit: ok: 0 vulnerabilities, 1 warning(s)", out)
        self.assertIn("audit: notice old-crate 0.1.0 RUSTSEC-2026-0002", out)

    def test_a_touched_lockfile_without_vulnerabilities_passes_and_lists_warnings(self):
        r = self.repo()
        r.write("Cargo.lock", EMPTY_LOCK + "# changed\n")
        r.answer(report(unmaintained=[("instant", "RUSTSEC-2024-0384")]))
        rc, out = r.run("--db", r.db)
        self.assertEqual(rc, 0, out)
        self.assertIn("audit: ok: 0 vulnerabilities, 1 warning(s)", out)
        self.assertIn("audit: unmaintained instant 0.1.0 RUSTSEC-2024-0384", out)

    def test_without_cargo_audit_only_a_gated_run_fails(self):
        r = self.repo()
        os.remove(os.path.join(r.stubs, "cargo-audit"))
        os.symlink(shutil.which("git"), os.path.join(r.stubs, "git"))
        env = {"PATH": r.stubs}
        rc, out = r.run("--db", r.db, env=env, stubbed=False)
        self.assertEqual(rc, 0, out)
        self.assertIn("cargo-audit is not installed", out)
        r.write("Cargo.lock", EMPTY_LOCK + "# changed\n")
        rc, out = r.run("--db", r.db, env=env, stubbed=False)
        self.assertEqual(rc, 2, out)


class Database(Case):
    def test_a_database_fetched_today_is_not_fetched_again(self):
        r = self.repo()
        r.run("--db", r.db)
        self.assertEqual(len(r.calls()), 1, r.calls())
        self.assertIn("--no-fetch --stale", r.calls()[0])

    def test_a_day_old_database_is_fetched(self):
        r = self.repo()
        r.fetched(hours_ago=25)
        rc, out = r.run("--db", r.db)
        self.assertEqual(rc, 0, out)
        self.assertEqual(len(r.calls()), 1, r.calls())
        self.assertNotIn("--no-fetch", r.calls()[0])

    def test_a_failed_fetch_audits_the_local_copy(self):
        r = self.repo()
        r.fetched(hours_ago=24 * 30)
        r.write("Cargo.lock", EMPTY_LOCK + "# changed\n")
        r.answer(report(vulns=[("rustls", "RUSTSEC-2026-0001")]))
        for error in ("error: couldn't fetch advisory database: offline",
                      "error: git operation failed: the lock is held by another process"):
            with self.subTest(error):
                if os.path.exists(r.log):
                    os.remove(r.log)
                rc, out = r.run("--db", r.db, env={"STUB_FETCH_FAILS": error})
                self.assertEqual(rc, 1, out)
                self.assertEqual(len(r.calls()), 2, r.calls())
                self.assertIn("--no-fetch --stale", r.calls()[1])
                self.assertIn("fetched 30 d ago, fetch failed, local copy used", out)

    def test_a_fetch_that_hangs_is_cut_off_and_the_local_copy_audited(self):
        r = self.repo()
        r.fetched(hours_ago=25)
        r.write("Cargo.lock", EMPTY_LOCK + "# changed\n")
        r.answer(report(vulns=[("rustls", "RUSTSEC-2026-0001")]))
        started = time.monotonic()
        rc, out = r.run("--db", r.db, "--fetch-timeout", "1", env={"STUB_FETCH_SLEEPS": "30"})
        self.assertLess(time.monotonic() - started, 20, out)
        self.assertEqual(rc, 1, out)
        self.assertIn("--no-fetch --stale", r.calls()[-1])
        self.assertIn("fetch timed out after 1 s, local copy used", out)


class Report(Case):
    def test_the_report_prints_both_tools_and_never_fails(self):
        r = self.repo()
        r.write("Cargo.lock", EMPTY_LOCK + "# changed\n")
        r.answer(report(vulns=[("rustls", "RUSTSEC-2026-0001")]))
        rc, out = r.run("--db", r.db, "--report")
        self.assertEqual(rc, 0, out)
        self.assertIn("terminal report", out)
        self.assertIn("outdated: upgrade --dry-run --incompatible allow", out)
        rc, out = r.run("--db", r.db, "--report", "--outdated", "cargo-outdated")
        self.assertIn("outdated: outdated --workspace --root-deps-only", out)


class RealAudit(Case):
    def test_a_branch_adding_a_vulnerable_lock_entry_fails(self):
        db = os.path.join(os.environ.get("CARGO_HOME") or os.path.expanduser("~/.cargo"),
                          "advisory-db")
        if shutil.which("cargo-audit") is None or not os.path.isdir(db):
            self.skipTest("needs cargo-audit and its advisory database")
        r = self.repo()
        r.write("Cargo.lock", VULNERABLE_LOCK)
        rc, out = r.run("--db", db, stubbed=False)
        self.assertEqual(rc, 1, out)
        self.assertIn("vulnerable quick-xml 0.26.0 RUSTSEC-2026-0194", out)


if __name__ == "__main__":
    unittest.main()
