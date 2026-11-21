"""Checks of scripts/rust_comments.py (the lexer and the in-body rule) and of
scripts/comments.py (the ratchet) on throwaway git repositories.
"""
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPTS = os.path.join(HERE, "..")
SCRIPT = os.path.join(SCRIPTS, "comments.py")
sys.path.insert(0, SCRIPTS)
import rust_comments  # noqa: E402


def kinds(src):
    return [(c.kind, c.start, c.trailing) for c in rust_comments.scan(src)]


def banned_lines(src):
    return [c.start for c in rust_comments.banned(rust_comments.scan(src), src)]


class Lexer(unittest.TestCase):
    def test_doc_and_plain_kinds(self):
        src = "//! m\n/// o\n//// p\n// q\n/** d */\n/*! i */\n/*** b */\n/**/\n"
        self.assertEqual([k for k, _, _ in kinds(src)],
                         ["inner-doc", "outer-doc", "line", "line", "outer-doc", "inner-doc",
                          "block", "block"])

    def test_comment_markers_inside_literals_are_code(self):
        src = (
            'let a = "// not";\n'
            'let b = "esc \\" // still string";\n'
            'let c = r#"/* raw "// */ "#;\n'
            "let d = br\"//\";\n"
            "let e = '/';\n"
            "let f = b'\\'';\n"
            "fn g<'a>(x: &'a str) {} // real\n"
        )
        found = rust_comments.scan(src)
        self.assertEqual([(c.kind, c.start, c.trailing) for c in found], [("line", 7, True)])

    def test_block_comments_nest(self):
        src = "/* a /* b */ c */ let x = 1;\n"
        found = rust_comments.scan(src)
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0].text, "/* a /* b */ c */")

    def test_lifetime_is_not_a_char_literal(self):
        src = "struct S<'a>(&'a u8); // t\nlet c = 'x'; // u\n"
        self.assertEqual([s for _, s, _ in kinds(src)], [1, 2])


class Rule(unittest.TestCase):
    def test_lone_safety_and_adr_pointer_are_allowed(self):
        src = ("fn f() {\n    // SAFETY: the slice is non-empty\n    unsafe { x() };\n"
               "    // ADR-0051 §3\n    y();\n    // ADR-0009 §X7\n    z();\n}\n")
        self.assertEqual(banned_lines(src), [])

    def test_a_safety_note_over_safe_code_is_banned(self):
        src = "fn f() {\n    // SAFETY: retry until the peer answers\n    let x = 1;\n}\n"
        self.assertEqual(banned_lines(src), [2])

    def test_a_section_token_carrying_words_is_banned(self):
        for text in ("// ADR-0051 §3:deny", "// ADR-0051 §because-the-peer-retransmits"):
            with self.subTest(text=text):
                self.assertEqual(banned_lines(f"fn f() {{\n    {text}\n    y();\n}}\n"), [2])

    def test_a_crlf_pointer_is_allowed(self):
        self.assertEqual(banned_lines("fn f() {\r\n    // ADR-0051\r\n    y();\r\n}\r\n"), [])

    def test_a_pointer_with_text_after_it_is_banned(self):
        src = "fn f() {\n    // ADR-0051 §3: deny\n    y();\n}\n"
        self.assertEqual(banned_lines(src), [2])

    def test_a_pointer_with_a_neighbour_line_is_banned(self):
        src = "fn f() {\n    // ADR-0051\n    // and more\n    y();\n}\n"
        self.assertEqual(banned_lines(src), [2, 3])

    def test_trailing_pointer_and_block_are_banned(self):
        src = "let x = 1; // SAFETY: no\n/* ADR-0001 */\n"
        self.assertEqual(banned_lines(src), [1, 2])

    def test_doc_comments_are_not_banned(self):
        self.assertEqual(banned_lines("/// a\n//! b\n/** c */\nfn f() {}\n"), [])

    def test_plain_text_is_banned(self):
        self.assertEqual(banned_lines("fn f() {\n    // note\n    x();\n}\n"), [2])


class Strip(unittest.TestCase):
    def test_strip_drops_comment_lines_and_cuts_trailing(self):
        src = "/// doc\nfn f() {\n    // a\n    x(); // b\n    let s = \"// c\";\n}\n"
        self.assertEqual(rust_comments.strip(src), 'fn f() {\n    x();\n    let s = "// c";\n}\n')

    def test_strip_keeps_what_keep_accepts(self):
        src = "/// doc\n// plain\nfn f() {}\n"
        kept = rust_comments.strip(src, lambda c: c.kind in rust_comments.DOC_KINDS)
        self.assertEqual(kept, "/// doc\nfn f() {}\n")


class Repo:
    def __init__(self, allow):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.git("init", "-q")
        self.git("config", "maintenance.auto", "false")
        self.git("config", "gc.auto", "0")
        self.write("comments.allow", allow)

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

    def run(self, *flags):
        r = subprocess.run(["python3", SCRIPT, "-C", self.root, *flags], capture_output=True, text=True)
        return r.returncode, r.stdout + r.stderr


SETTINGS = "limit 0\ntest-limit 0\nscan src\n"


def body(n):
    return "fn f() {\n" + "".join(f"    // note {i}\n    x();\n" for i in range(n)) + "}\n"


class Ratchet(unittest.TestCase):
    def repo(self, allow):
        r = Repo(allow)
        self.addCleanup(r.tmp.cleanup)
        return r

    def test_an_unlisted_file_with_one_banned_comment_fails(self):
        repo = self.repo(SETTINGS)
        repo.write("src/a.rs", body(0))
        self.assertEqual(repo.run()[0], 0)
        repo.write("src/a.rs", body(1))
        rc, out = repo.run()
        self.assertEqual(rc, 1)
        self.assertIn("src/a.rs has 1 banned comment lines, budget 0", out)
        self.assertIn("src/a.rs:2: // note 0", out)

    def test_a_listed_file_may_not_grow(self):
        repo = self.repo(SETTINGS + "src/a.rs 2\n")
        repo.write("src/a.rs", body(2))
        self.assertEqual(repo.run()[0], 0)
        repo.write("src/a.rs", body(3))
        self.assertEqual(repo.run()[0], 1)

    def test_raising_an_entry_against_the_base_fails(self):
        repo = self.repo(SETTINGS + "src/a.rs 2\n")
        repo.write("src/a.rs", body(2))
        repo.commit()
        repo.write("comments.allow", SETTINGS + "src/a.rs 3\n")
        repo.write("src/a.rs", body(3))
        rc, out = repo.run("--base", "HEAD")
        self.assertEqual(rc, 1)
        self.assertIn("above its 2", out)

    def test_update_lowers_and_removes_entries(self):
        repo = self.repo(SETTINGS + "src/a.rs 3\nsrc/b.rs 2\n")
        repo.write("src/a.rs", body(1))
        repo.write("src/b.rs", body(0))
        repo.commit()
        self.assertEqual(repo.run()[0], 1)
        rc, out = repo.run("--update")
        self.assertEqual(rc, 0, out)
        self.assertEqual(repo.read("comments.allow"), SETTINGS + "src/a.rs 1\n")

    def test_staged_checks_the_index_blob(self):
        repo = self.repo(SETTINGS)
        repo.write("src/a.rs", body(0))
        repo.commit()
        repo.write("src/a.rs", body(1))
        repo.git("add", "src/a.rs")
        repo.write("src/a.rs", body(0))
        self.assertEqual(repo.run("--staged")[0], 1)


if __name__ == "__main__":
    unittest.main()
