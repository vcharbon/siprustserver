"""Checks of scripts/file-size.py on throwaway git repositories.

The real allowlist's settings (limit, ext, scan, exclude) and its first entry
are probed too: `FILE_SIZE_ALLOW` names that allowlist, by default this
repository's `file-size.allow`.
"""
import os
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "..", "file-size.py")
REAL_ALLOW = os.environ.get("FILE_SIZE_ALLOW", os.path.join(HERE, "..", "..", "file-size.allow"))


def lines(n):
    return "".join(f"// {i}\n" for i in range(n))


class Repo:
    def __init__(self, allow):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.git("init", "-q")
        self.write("file-size.allow", allow)

    def git(self, *args):
        subprocess.run(["git", "-C", self.root, *args], check=True, capture_output=True)

    def write(self, rel, text):
        path = os.path.join(self.root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)

    def read(self, rel):
        with open(os.path.join(self.root, rel)) as f:
            return f.read()

    def run(self, *flags):
        r = subprocess.run(["python3", SCRIPT, "-C", self.root, *flags],
                           capture_output=True, text=True)
        return r.returncode, r.stdout + r.stderr


class Case(unittest.TestCase):
    def repo(self, allow):
        r = Repo(allow)
        self.addCleanup(r.tmp.cleanup)
        return r


SETTINGS = "limit 500\ntest-limit 800\nscan src\nexclude src/gen/*\n"


class Ratchet(Case):
    def test_listed_file_grown_by_one_line_fails(self):
        repo = self.repo(SETTINGS + "src/big.rs 600\n")
        repo.write("src/big.rs", lines(600))
        self.assertEqual(repo.run()[0], 0)
        repo.write("src/big.rs", lines(601))
        rc, out = repo.run()
        self.assertEqual(rc, 1)
        self.assertIn("src/big.rs has 601 lines, budget 600 (allowlisted): split it", out)
        self.assertNotIn("raise", out)

    def test_new_file_over_the_limit_fails(self):
        repo = self.repo(SETTINGS)
        repo.write("src/new.rs", lines(500))
        self.assertEqual(repo.run()[0], 0)
        repo.write("src/new.rs", lines(501))
        rc, out = repo.run()
        self.assertEqual(rc, 1)
        self.assertIn("src/new.rs has 501 lines, budget 500: split it", out)

    def test_a_test_file_has_the_test_limit(self):
        for rel in ("src/a/tests.rs", "src/tests/it/main.rs", "src/a/quota_tests.rs"):
            repo = self.repo(SETTINGS)
            repo.write(rel, lines(800))
            self.assertEqual(repo.run(), (0, ""), rel)
            repo.write(rel, lines(801))
            rc, out = repo.run()
            self.assertEqual(rc, 1, rel)
            self.assertIn(f"{rel} has 801 lines, budget 800 (test file): split it", out)

    def test_test_globs_replace_the_default(self):
        repo = self.repo(SETTINGS + "test src/spec/*\n")
        repo.write("src/spec/a.rs", lines(800))
        repo.write("src/a/tests.rs", lines(501))
        rc, out = repo.run()
        self.assertEqual(rc, 1)
        self.assertNotIn("src/spec/a.rs", out)
        self.assertIn("src/a/tests.rs has 501 lines, budget 500: split it", out)

    def test_last_line_without_newline_counts(self):
        repo = self.repo(SETTINGS)
        repo.write("src/a.rs", lines(500) + "x")
        self.assertEqual(repo.run()[0], 1)

    def test_out_of_scope_files_are_not_scanned(self):
        repo = self.repo(SETTINGS)
        repo.write(".gitignore", "target/\n")
        repo.write("src/gen/huge.rs", lines(900))
        repo.write("src/target/huge.rs", lines(900))
        repo.write("src/notes.md", lines(900))
        repo.write("src/app.ts", lines(900))
        repo.write("other/huge.rs", lines(900))
        self.assertEqual(repo.run(), (0, ""))

    def test_stale_entries_fail_and_update_lowers_or_removes_them(self):
        repo = self.repo(SETTINGS + "src/a.rs 700  # keep this note\nsrc/b.rs 700\nsrc/c.rs 700\n"
                    "src/d.rs 700\n")
        repo.write("src/a.rs", lines(650))
        repo.write("src/b.rs", lines(400))
        repo.write("src/d.rs", lines(701))
        rc, out = repo.run()
        self.assertEqual(rc, 1)
        self.assertIn("lists src/a.rs at 700, it has 650 lines", out)
        self.assertIn("lists src/b.rs at 700, it is under the limit", out)
        self.assertIn("lists src/c.rs, which is gone", out)
        rc, out = repo.run("--update")
        self.assertEqual(rc, 1, out)  # src/d.rs grew: --update never raises a budget
        self.assertEqual(repo.read("file-size.allow"),
                         SETTINGS + "src/a.rs 650  # keep this note\nsrc/d.rs 700\n")
        repo.write("src/d.rs", lines(700))
        self.assertEqual(repo.run()[0], 0)

    def test_update_adds_no_entry(self):
        repo = self.repo(SETTINGS)
        repo.write("src/new.rs", lines(501))
        self.assertEqual(repo.run("--update")[0], 1)
        self.assertEqual(repo.read("file-size.allow"), SETTINGS)

    def test_staged_reads_the_index_not_the_worktree(self):
        repo = self.repo(SETTINGS + "src/big.rs 600\n")
        repo.write("src/big.rs", lines(601))
        repo.write("src/ok.rs", lines(10))
        repo.git("add", "file-size.allow", "src/ok.rs")
        self.assertEqual(repo.run("--staged")[0], 0)
        repo.git("add", "src/big.rs")
        rc, out = repo.run("--staged")
        self.assertEqual(rc, 1)
        self.assertIn("src/big.rs has 601 lines", out)
        repo.write("src/big.rs", lines(10))
        self.assertEqual(repo.run("--staged")[0], 1)

    def test_report_lists_files_over_the_limit(self):
        repo = self.repo(SETTINGS + "src/big.rs 600\n")
        repo.write("src/big.rs", lines(600))
        repo.write("src/new.rs", lines(501))
        rc, out = repo.run("--report")
        self.assertEqual(rc, 0)
        self.assertIn("600  src/big.rs  (budget 600)", out)
        self.assertIn("501  src/new.rs  (over the 500 limit, unlisted)", out)

    def test_unreadable_allowlist_line_is_a_setup_error(self):
        repo = self.repo(SETTINGS + "src/a.rs lots\n")
        self.assertEqual(repo.run()[0], 2)


class RealAllowlist(Case):
    """The repository's own settings catch both probes."""

    def setUp(self):
        with open(REAL_ALLOW) as f:
            text = f.read()
        self.settings, self.entries = "", []
        for raw in text.splitlines():
            words = raw.split("#", 1)[0].split()
            if words and words[0] in ("limit", "test-limit", "test", "ext", "scan", "exclude"):
                self.settings += raw + "\n"
            elif len(words) == 2:
                self.entries.append((words[0], int(words[1])))
        setting = {w.split()[0]: w.split()[1] for w in self.settings.splitlines()}
        self.limit = int(setting.get("limit", 500))
        self.test_limit = int(setting.get("test-limit", 800))
        scan = [w.split()[1] for w in self.settings.splitlines() if w.split()[0] == "scan"]
        self.new_file = os.path.join(scan[0] if scan else "src", "file_size_probe.rs")
        self.new_test = os.path.join(scan[0] if scan else "src", "file_size_probe", "tests.rs")

    def test_a_listed_file_grown_by_one_line_fails(self):
        if not self.entries:
            self.skipTest("the allowlist is empty")
        rel, budget = self.entries[0]
        repo = self.repo(self.settings + f"{rel} {budget}\n")
        repo.write(rel, lines(budget))
        self.assertEqual(repo.run(), (0, ""))
        repo.write(rel, lines(budget + 1))
        rc, out = repo.run()
        self.assertEqual(rc, 1)
        self.assertIn(f"{rel} has {budget + 1} lines, budget {budget} (allowlisted): split it", out)

    def test_a_new_file_over_the_limit_fails(self):
        repo = self.repo(self.settings)
        repo.write(self.new_file, lines(self.limit + 1))
        rc, out = repo.run()
        self.assertEqual(rc, 1)
        self.assertIn(f"{self.new_file} has {self.limit + 1} lines", out)

    def test_a_new_test_file_over_the_test_limit_fails(self):
        repo = self.repo(self.settings)
        repo.write(self.new_test, lines(self.test_limit))
        self.assertEqual(repo.run(), (0, ""))
        repo.write(self.new_test, lines(self.test_limit + 1))
        rc, out = repo.run()
        self.assertEqual(rc, 1)
        self.assertIn(f"{self.new_test} has {self.test_limit + 1} lines", out)


if __name__ == "__main__":
    unittest.main()
