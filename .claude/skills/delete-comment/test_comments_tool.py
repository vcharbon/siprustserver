"""Checks of the skill's comments.py: `apply` and `same-code` on throwaway
repositories (under a second). `just comments-check` runs them.
"""
import json
import os
import subprocess
import tempfile
import unittest

TOOL = os.path.join(os.path.dirname(os.path.abspath(__file__)), "comments.py")


class Repo:
    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.git("init", "-q")
        self.git("config", "maintenance.auto", "false")
        self.git("config", "gc.auto", "0")

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

    def commit(self):
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "base")

    def run(self, *args):
        r = subprocess.run(["python3", TOOL, *args], capture_output=True, text=True, cwd=self.root)
        return r.returncode, r.stdout + r.stderr

    def apply(self, rows):
        self.write("rows.jsonl", "".join(json.dumps(r) + "\n" for r in rows))
        return self.run("apply", "-C", self.root, os.path.join(self.root, "rows.jsonl"))


class Case(unittest.TestCase):
    def repo(self):
        r = Repo()
        self.addCleanup(r.tmp.cleanup)
        return r


class SameCode(Case):
    def same(self, before, after):
        repo = self.repo()
        repo.write("x.rs", before)
        repo.commit()
        repo.write("x.rs", after)
        return repo.run("same-code", "-C", repo.root, "--base", "HEAD", "x.rs")[0]

    def test_a_comment_only_edit_and_a_reflow_are_the_same_code(self):
        before = "enum E {\n    /// a\n    A { x: u8, y: u8 },\n}\nfn f() { g(1, 2) } // t\n"
        after = "enum E {\n    A {\n        x: u8,\n        y: u8,\n    },\n}\nfn f() {\n    g(\n        1,\n        2,\n    )\n}\n"
        self.assertEqual(self.same(before, after), 0)

    def test_a_changed_string_literal_is_a_code_change(self):
        self.assertEqual(self.same('fn f() { let s = "SIP/2.0 200 OK"; }\n',
                                   'fn f() { let s = "SIP/2.0200OK"; }\n'), 1)

    def test_a_trailing_comma_inside_a_literal_is_a_code_change(self):
        self.assertEqual(self.same('fn f() { let t = "a,]"; }\n', 'fn f() { let t = "a]"; }\n'), 1)

    def test_a_one_tuple_losing_its_comma_is_a_code_change(self):
        self.assertEqual(self.same("fn f() -> (u8,) { (1,) }\n", "fn f() -> (u8) { (1) }\n"), 1)

    def test_merged_words_are_a_code_change(self):
        self.assertEqual(self.same("fn f() { let x = 1; }\n", "fn f() { letx = 1; }\n"), 1)

    def test_a_one_tuple_of_a_comma_holding_element_losing_its_comma_is_a_code_change(self):
        for before, after in (("type T = (HashMap<K, V>,);\n", "type T = (HashMap<K, V>);\n"),
                              ("fn f() { let t = (|a, b| a + b,); }\n",
                               "fn f() { let t = (|a, b| a + b); }\n")):
            with self.subTest(before=before):
                self.assertEqual(self.same(before, after), 1)

    def test_split_operators_are_a_code_change(self):
        self.assertEqual(self.same("fn f(a: bool, b: bool) -> bool { a && b }\n",
                                   "fn f(a: bool, b: bool) -> bool { a & &b }\n"), 1)

    def test_blank_and_trailing_blanks_inside_a_literal_are_code(self):
        for before, after in (('const S: &str = "Via: a\n\nbody";\n', 'const S: &str = "Via: a\nbody";\n'),
                              ('const S: &str = r"a\nb";\n', 'const S: &str = r"a\n\n\nb";\n'),
                              ('const S: &str = "a\n   \nb";\n', 'const S: &str = "a\n\nb";\n'),
                              ('fn f() { g(/* c */ "a  \nb"); }\n', 'fn f() { g("a\nb"); }\n')):
            with self.subTest(before=before):
                self.assertEqual(self.same(before, after), 1)

    def test_a_file_rustfmt_cannot_format_is_not_proved(self):
        self.assertEqual(self.same("fn f( {\n// c\n}\n", "fn f( {\n}\n"), 1)

    def test_a_list_collapsed_once_its_comment_goes_is_the_same_code(self):
        before = "fn f() {\n    g(\n        // why\n        long_argument_name,\n    );\n}\n"
        after = "fn f() {\n    g(long_argument_name);\n}\n"
        self.assertEqual(self.same(before, after), 0)


class Apply(Case):
    def test_a_trailing_comment_is_cut_at_its_own_column(self):
        repo = self.repo()
        repo.write("src/a.rs", 'fn f() {\n    let s = "x // c"; // c\n}\n')
        rc, out = repo.apply([{"id": "src/a.rs:2", "action": "delete"}])
        self.assertEqual(rc, 0, out)
        self.assertEqual(repo.read("src/a.rs"), 'fn f() {\n    let s = "x // c";\n}\n')

    def test_a_block_comment_sharing_a_line_with_code_is_cut_alone(self):
        repo = self.repo()
        src = "fn f() {\n    g(/* timeout */ 5);\n    /* old */ let x = 1;\n    let y = 1; /* a\n       b */\n}\n"
        repo.write("src/a.rs", src)
        rc, out = repo.apply([{"id": "src/a.rs:2", "action": "delete"},
                              {"id": "src/a.rs:3", "action": "delete"},
                              {"id": "src/a.rs:4", "action": "delete"}])
        self.assertEqual(rc, 0, out)
        self.assertEqual(repo.read("src/a.rs"), "fn f() {\n    g( 5);\n    let x = 1;\n    let y = 1;\n}\n")

    def test_two_blocks_on_one_line_have_ids_of_their_own(self):
        repo = self.repo()
        repo.write("src/a.rs", "fn f() {\n    g(/* a */ 5); // b\n}\n")
        rc, out = repo.run("list", "src")
        ids = [json.loads(l)["id"] for l in out.splitlines()]
        self.assertEqual(ids, ["src/a.rs:2:7", "src/a.rs:2:19"])
        rc, out = repo.apply([{"id": i, "action": "delete"} for i in ids])
        self.assertEqual(rc, 0, out)
        self.assertEqual(repo.read("src/a.rs"), "fn f() {\n    g( 5);\n}\n")

    def test_overlapping_edits_edit_nothing(self):
        repo = self.repo()
        src = "fn x() {\n// one\n// two\n// three\na();\n// contract\nb();\n}\n"
        repo.write("src/a.rs", src)
        rc, out = repo.apply([{"id": "src/a.rs:2", "action": "delete"},
                              {"id": "src/a.rs:6", "action": "to-doc", "insert_before": 3, "text": "/// C."}])
        self.assertEqual(rc, 2)
        self.assertIn("overlap", out)
        self.assertEqual(repo.read("src/a.rs"), src)
        src2 = "fn x() {\n    /* a */ /* b */x();\n}\n"
        repo.write("src/b.rs", src2)
        rc, out = repo.apply([{"id": "src/b.rs:2:5", "action": "delete"},
                              {"id": "src/b.rs:2:13", "action": "delete"}])
        self.assertEqual(rc, 2)
        self.assertEqual(repo.read("src/b.rs"), src2)

    def test_an_insert_before_outside_the_file_edits_nothing(self):
        repo = self.repo()
        src = "fn x() {\n    // c\n    a();\n}\n"
        repo.write("src/a.rs", src)
        for at in (0, 99):
            with self.subTest(at=at):
                rc, out = repo.apply([{"id": "src/a.rs:2", "action": "to-doc", "insert_before": at,
                                       "text": "/// C."}])
                self.assertEqual(rc, 2)
                self.assertEqual(repo.read("src/a.rs"), src)

    def test_an_id_classified_twice_edits_nothing(self):
        repo = self.repo()
        src = "fn f() {\n    x();\n    // c\n    let a = 1;\n}\n"
        repo.write("src/a.rs", src)
        rc, out = repo.apply([{"id": "src/a.rs:3", "action": "delete"}] * 2)
        self.assertEqual(rc, 2)
        self.assertIn("classified twice", out)
        self.assertEqual(repo.read("src/a.rs"), src)

    def test_an_id_naming_no_block_edits_nothing(self):
        repo = self.repo()
        repo.write("src/a.rs", "fn f() {}\n")
        rc, out = repo.apply([{"id": "src/a.rs:9", "action": "delete"}])
        self.assertEqual(rc, 2)
        self.assertIn("name no block", out)

    def test_to_docs_sharing_a_line_keep_file_order(self):
        repo = self.repo()
        repo.write("src/b.rs", "fn x() {\n    // one\n    a();\n    // two\n    b();\n}\n")
        rc, out = repo.apply([
            {"id": "src/b.rs:4", "action": "to-doc", "insert_before": 1, "text": "/// Two."},
            {"id": "src/b.rs:2", "action": "to-doc", "insert_before": 1, "text": "/// One."},
        ])
        self.assertEqual(rc, 0, out)
        self.assertEqual(repo.read("src/b.rs"), "/// One.\n/// Two.\nfn x() {\n    a();\n    b();\n}\n")

    def test_rewrite_and_pointer_replace_their_lines(self):
        repo = self.repo()
        repo.write("src/c.rs", "/// Returns x.\n/// Old.\nfn x() -> u8 {\n    // why\n    1\n}\n")
        rc, out = repo.apply([
            {"id": "src/c.rs:1", "action": "rewrite", "text": "/// The answer, 1."},
            {"id": "src/c.rs:4", "action": "adr-pointer", "text": "    // ADR-0001"},
        ])
        self.assertEqual(rc, 0, out)
        self.assertEqual(repo.read("src/c.rs"), "/// The answer, 1.\nfn x() -> u8 {\n    // ADR-0001\n    1\n}\n")


if __name__ == "__main__":
    unittest.main()
