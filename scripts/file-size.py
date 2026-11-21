#!/usr/bin/env python3
"""File-size budget of first-party sources: a ratchet on line counts.

  file-size.py [-C DIR] [--allow FILE] [--base REV] [--staged | --update | --report]

A scanned file may have at most `limit` lines, a test file `test-limit`,
unless the allowlist names it with a budget: a listed file may not grow past
its budget. The allowlist (`file-size.allow` at the root of DIR's repository
by default) holds the settings and the entries:

  limit 500                      the line limit of an unlisted file
  test-limit 800                 the line limit of an unlisted test file
  test */tests/* */tests.rs      path globs of the test files (by default
  test *_tests.rs                these three and `tests/*`)
  ext .rs                        the extensions scanned (default .rs)
  scan crates xtask              the directories scanned (default: the root)
  exclude crates/a/src/gen/*     a path glob left out (generated, fixtures)
  crates/a/src/big.rs 812        an entry: a file over the limit, its budget

`#` starts a comment. The files scanned are those git sees under the scan
directories: tracked, or untracked and not ignored. A scan directory is a plain
path (no pathspec magic).

The allowlist itself only ever tightens: against its version at a base (REV,
by default the merge-base of HEAD and `develop`, else HEAD; HEAD under
--staged), a new entry, a raised number or a changed setting line other
than `limit` and `test-limit` fails.
The one new entry allowed is a file git sees renamed from a listed one in the
same diff (`-M`), the old entry gone and the number no higher than its own;
the changed settings allowed are a `scan` or `exclude` path following a scan
directory git sees renamed whole, none of its files left behind, and `scan`
directories added to a base that already scans a list (scripts/ratchet.py).

  (no flag)  every scanned file against its budget, and the allowlist
             against the base; an entry above its file's size, or naming a
             file under the limit or gone, is stale and fails too
             (`--update` fixes it)
  --staged   the staged blobs of the staged files only, and the staged
             allowlist against HEAD's (a pre-commit hook), following the
             renames since the base (a rename committed before the allowlist
             follows it); stale entries are not reported
  --update   lowers each entry to its file's size, moves the entry of a
             renamed file to its new path, removes the entries of files under
             the limit or gone; never raises nor adds one
  --report   every scanned file over its limit, largest first; exits 0

Exit 1 on a violation, 2 on a usage or setup error.
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ratchet  # noqa: E402
from ratchet import SetupError, git, rev_exists, default_base, loosened  # noqa: E402

LIMIT_DEFAULT = 500
TEST_LIMIT_DEFAULT = 800
UNIT = "lines"
# The settings that may change: a limit is raised or lowered deliberately, in review.
LIMITS = ("limit", "test-limit")


def count_lines(data):
    return data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)


def scanned(root, allow):
    sizes = {}
    for rel in ratchet.listed(root, allow):
        with open(os.path.join(root, rel), "rb") as f:
            sizes[rel] = count_lines(f.read())
    return sizes


def over_budget(sizes, allow):
    bad = []
    for rel, n in sorted(sizes.items()):
        budget = allow.budget(rel)
        if n > budget:
            listed = " (allowlisted)" if rel in allow.entries else (
                " (test file)" if allow.is_test(rel) else "")
            bad.append(f"{rel} has {n} lines, budget {budget}{listed}: split it")
    return bad


BASE_HINT = "the allowlist only tightens: a file over its limit is split, never listed or raised"
SPLIT_HINT = "a file over its budget is split into modules; its budget never goes up"


def report(violations, hint=None):
    for v in violations:
        print(f"file-size: {v}", file=sys.stderr)
    if hint:
        print(f"file-size: {hint}", file=sys.stderr)
    return 1 if violations else 0


def cmd_check(root, allow, base):
    sizes = scanned(root, allow)
    grown = over_budget(sizes, allow)
    old = ratchet.stale(sizes, allow, UNIT)
    loose = loosened(root, allow, base, base, False, LIMITS)
    rc = report(grown, SPLIT_HINT if grown else None)
    rc = report(loose, BASE_HINT if loose else None) or rc
    return report(old, "run `file-size.py --update` and commit the allowlist" if old else None) or rc


def cmd_staged(root, allow_rel):
    allow = ratchet.staged_allow(root, allow_rel, LIMIT_DEFAULT, TEST_LIMIT_DEFAULT)
    sizes = {rel: count_lines(git(root, "cat-file", "blob", f":{rel}", binary=True))
             for rel in ratchet.staged(root, allow)}
    grown = over_budget(sizes, allow)
    base = "HEAD" if rev_exists(root, "HEAD") else None
    loose = loosened(root, allow, base, default_base(root), True, LIMITS)
    rc = report(grown, SPLIT_HINT if grown else None)
    return report(loose, BASE_HINT if loose else None) or rc


def cmd_update(root, allow, allow_file, base):
    sizes = scanned(root, allow)
    out, changed = ratchet.updated(allow, sizes, ratchet.renames(root, base, False), UNIT)
    if changed:
        with open(allow_file, "w") as f:
            f.write("\n".join(out) + "\n")
    for c in changed:
        print(f"file-size: {c}")
    allow = load_allow(root, os.path.relpath(allow_file, root))
    grown = over_budget(sizes, allow)
    loose = loosened(root, allow, base, base, False, LIMITS)
    rc = report(grown, SPLIT_HINT if grown else None)
    return report(loose, BASE_HINT if loose else None) or rc


def cmd_report(root, allow):
    sizes = scanned(root, allow)
    rows = sorted(((n, rel) for rel, n in sizes.items() if n > allow.limit_of(rel)),
                  reverse=True)
    for n, rel in rows:
        budget = allow.entries.get(rel)
        state = f"over the {allow.limit_of(rel)} limit, unlisted" if budget is None else (
            f"budget {budget}" + (", OVER" if n > budget else ""))
        print(f"{n:7d}  {rel}  ({state})")
    print(f"{len(rows)} file(s) over their limit ({allow.limit}, tests {allow.test_limit}); "
          f"{len(allow.entries)} allowlist entries")
    return 0


def load_allow(root, allow_rel):
    return ratchet.load_allow(root, allow_rel, LIMIT_DEFAULT, TEST_LIMIT_DEFAULT)


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
    p.add_argument("--allow", default="file-size.allow", help="relative to the root")
    p.add_argument("--base", help="the revision the allowlist is held to (not with --staged)")
    mode = p.add_mutually_exclusive_group()
    mode.add_argument("--staged", action="store_true")
    mode.add_argument("--update", action="store_true")
    mode.add_argument("--report", action="store_true")
    try:
        return run(p.parse_args())
    except SetupError as e:
        print(f"file-size: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
