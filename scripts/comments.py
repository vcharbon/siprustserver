#!/usr/bin/env python3
"""The in-body comment ban of Rust sources: a ratchet on banned comment lines.

  comments.py [-C DIR] [--allow FILE] [--base REV] [--staged | --update | --report]

A comment is a doc comment (`///`, `//!`, `/** */`, `/*! */`) stating its
item's contract. Every other comment is banned, trailing ones included, except
a one-line `// SAFETY: ...` or a bare one-line ADR pointer (`// ADR-0051 §3`)
standing alone, with no comment line above or below it
(scripts/rust_comments.py has the lexer and the rule).

A scanned file may hold `limit` banned comment lines, a test file
`test-limit` (both 0), unless the allowlist names it with a budget: a listed
file may not grow past it. The allowlist (`comments.allow` at the root of
DIR's repository by default) has file-size.py's settings and entries, and only
ever tightens against its version at a base (REV, by default the merge-base of
HEAD and `develop`; HEAD under --staged): scripts/ratchet.py.

  (no flag)  every scanned file against its budget, and the allowlist
             against the base; an entry above its file's count, or naming a
             file at 0 or gone, is stale and fails too (`--update` fixes it)
  --staged   the staged blobs of the staged files only, and the staged
             allowlist against HEAD's (a pre-commit hook)
  --update   lowers each entry to its file's count, moves the entry of a
             renamed file, removes the entries of files at 0 or gone
  --report   every file holding banned comments, most first, with its budget

Exit 1 on a violation, 2 on a usage or setup error.
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ratchet  # noqa: E402
import rust_comments  # noqa: E402
from ratchet import SetupError, git, rev_exists, default_base, loosened  # noqa: E402

UNIT = "banned comment lines"
BASE_HINT = "the allowlist only tightens: a banned comment is removed, never listed or raised"
FIX_HINT = ("a comment is a doc comment stating its item's contract; unclear code takes a "
            "better name or an extracted function, a decision an ADR and a one-line pointer")


def banned_lines(text):
    """`[(line, text)]` of the banned comment lines of a source."""
    out = []
    for c in rust_comments.banned(rust_comments.scan(text)):
        for k, line in enumerate(c.text.split("\n")):
            out.append((c.start + k, line.strip()))
    return out


def measure_files(root, rels, read):
    found = {}
    for rel in rels:
        found[rel] = banned_lines(read(rel))
    return found


def read_worktree(root):
    def read(rel):
        with open(os.path.join(root, rel), encoding="utf-8", errors="replace") as f:
            return f.read()
    return read


def over_budget(found, allow, verbose):
    bad = []
    for rel, hits in sorted(found.items()):
        budget = allow.budget(rel)
        if len(hits) > budget:
            listed = " (allowlisted)" if rel in allow.entries else ""
            bad.append(f"{rel} has {len(hits)} {UNIT}, budget {budget}{listed}")
            if verbose:
                bad.extend(f"  {rel}:{ln}: {t[:100]}" for ln, t in hits)
    return bad


def report(violations, hint=None):
    for v in violations:
        print(f"comments: {v}", file=sys.stderr)
    if hint:
        print(f"comments: {hint}", file=sys.stderr)
    return 1 if violations else 0


def counts(found):
    return {rel: len(hits) for rel, hits in found.items()}


def cmd_check(root, allow, base):
    found = measure_files(root, ratchet.listed(root, allow), read_worktree(root))
    grown = over_budget(found, allow, True)
    old = ratchet.stale(counts(found), allow, UNIT)
    loose = loosened(root, allow, base, base, False)
    rc = report(grown, FIX_HINT if grown else None)
    rc = report(loose, BASE_HINT if loose else None) or rc
    return report(old, "run `comments.py --update` and commit the allowlist" if old else None) or rc


def cmd_staged(root, allow_rel):
    allow = ratchet.staged_allow(root, allow_rel, 0, 0)
    found = measure_files(
        root, ratchet.staged(root, allow),
        lambda rel: git(root, "cat-file", "blob", f":{rel}", binary=True).decode(errors="replace"))
    grown = over_budget(found, allow, True)
    base = "HEAD" if rev_exists(root, "HEAD") else None
    loose = loosened(root, allow, base, default_base(root), True)
    rc = report(grown, FIX_HINT if grown else None)
    return report(loose, BASE_HINT if loose else None) or rc


def cmd_update(root, allow, allow_file, base):
    found = counts(measure_files(root, ratchet.listed(root, allow), read_worktree(root)))
    out, changed = ratchet.updated(allow, found, ratchet.renames(root, base, False), UNIT)
    if changed:
        with open(allow_file, "w") as f:
            f.write("\n".join(out) + "\n")
    for c in changed:
        print(f"comments: {c}")
    allow = load_allow(root, os.path.relpath(allow_file, root))
    found_all = measure_files(root, ratchet.listed(root, allow), read_worktree(root))
    grown = over_budget(found_all, allow, False)
    loose = loosened(root, allow, base, base, False)
    rc = report(grown, FIX_HINT if grown else None)
    return report(loose, BASE_HINT if loose else None) or rc


def cmd_report(root, allow):
    found = counts(measure_files(root, ratchet.listed(root, allow), read_worktree(root)))
    rows = sorted(((n, rel) for rel, n in found.items() if n), reverse=True)
    for n, rel in rows:
        budget = allow.budget(rel)
        print(f"{n:7d}  {rel}  (budget {budget}{', OVER' if n > budget else ''})")
    print(f"{len(rows)} file(s) hold {sum(n for n, _ in rows)} {UNIT}; "
          f"{len(allow.entries)} allowlist entries")
    return 0


def load_allow(root, allow_rel):
    return ratchet.load_allow(root, allow_rel, 0, 0)


def run(a):
    root = git(a.root, "rev-parse", "--show-toplevel").strip()
    if a.staged and a.base:
        raise SetupError("--staged holds the allowlist to HEAD; --base does not apply")
    if a.staged:
        return cmd_staged(root, a.allow)
    if a.base and not rev_exists(root, a.base):
        raise SetupError(f"--base {a.base} is no commit")
    base = a.base or default_base(root)
    allow = load_allow(root, a.allow)
    if a.update:
        return cmd_update(root, allow, os.path.join(root, a.allow), base)
    if a.report:
        return cmd_report(root, allow)
    return cmd_check(root, allow, base)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("-C", dest="root", default=".", help="any directory of the repository")
    p.add_argument("--allow", default="comments.allow", help="relative to the root")
    p.add_argument("--base", help="the revision the allowlist is held to (not with --staged)")
    mode = p.add_mutually_exclusive_group()
    mode.add_argument("--staged", action="store_true")
    mode.add_argument("--update", action="store_true")
    mode.add_argument("--report", action="store_true")
    try:
        return run(p.parse_args())
    except SetupError as e:
        print(f"comments: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
