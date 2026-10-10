#!/usr/bin/env python3
"""File-size budget of first-party sources: a ratchet on line counts.

  file-size.py [-C DIR] [--allow FILE] [--staged | --update | --report]

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
directories: tracked, or untracked and not ignored.

  (no flag)  every scanned file against its budget; an entry above its
             file's size, or naming a file under the limit or gone, is
             stale and fails too (`--update` fixes it)
  --staged   the staged blobs of the staged files only (a pre-commit hook);
             stale entries are not reported
  --update   lowers each entry to its file's size, removes the entries of
             files under the limit or gone; never raises nor adds one
  --report   every scanned file over its limit, largest first; exits 0

Exit 1 on a violation, 2 on a usage or setup error.
"""
import argparse
import fnmatch
import os
import subprocess
import sys

LIMIT_DEFAULT = 500
TEST_LIMIT_DEFAULT = 800
TEST_DEFAULT = ["tests/*", "*/tests/*", "*/tests.rs", "*_tests.rs"]
EXT_DEFAULT = (".rs",)


class Allow:
    def __init__(self, text, path):
        self.path = path
        self.limit = LIMIT_DEFAULT
        self.test_limit = TEST_LIMIT_DEFAULT
        self.test = []
        self.ext = EXT_DEFAULT
        self.scan = []
        self.exclude = []
        self.entries = {}
        self.lines = text.splitlines()
        for n, raw in enumerate(self.lines, 1):
            words = raw.split("#", 1)[0].split()
            if not words:
                continue
            key, rest = words[0], words[1:]
            if key == "limit" and len(rest) == 1 and rest[0].isdigit():
                self.limit = int(rest[0])
            elif key == "test-limit" and len(rest) == 1 and rest[0].isdigit():
                self.test_limit = int(rest[0])
            elif key == "test" and rest:
                self.test.extend(rest)
            elif key == "ext" and rest:
                self.ext = tuple(rest)
            elif key == "scan" and rest:
                self.scan.extend(r.rstrip("/") for r in rest)
            elif key == "exclude" and len(rest) == 1:
                self.exclude.append(rest[0])
            elif len(rest) == 1 and rest[0].isdigit():
                if key in self.entries:
                    fail_setup(f"{path}:{n}: {key} is listed twice")
                self.entries[key] = int(rest[0])
            else:
                fail_setup(f"{path}:{n}: unreadable line: {raw.strip()}")
        self.test = self.test or TEST_DEFAULT

    def in_scope(self, rel):
        if not rel.endswith(self.ext):
            return False
        if self.scan and not any(rel.startswith(d + "/") for d in self.scan):
            return False
        return not any(fnmatch.fnmatchcase(rel, g) for g in self.exclude)

    def is_test(self, rel):
        return any(fnmatch.fnmatchcase(rel, g) for g in self.test)

    def limit_of(self, rel):
        return self.test_limit if self.is_test(rel) else self.limit

    def budget(self, rel):
        return self.entries.get(rel, self.limit_of(rel))


def fail_setup(msg):
    print(f"file-size: {msg}", file=sys.stderr)
    sys.exit(2)


def git(root, *args, binary=False):
    out = subprocess.run(["git", "-C", root, *args], capture_output=True)
    if out.returncode != 0:
        fail_setup(f"git {' '.join(args)}: {out.stderr.decode(errors='replace').strip()}")
    return out.stdout if binary else out.stdout.decode()


def count_lines(data):
    return data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)


def scanned(root, allow):
    listed = git(root, "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--",
                 *(allow.scan or ["."]))
    sizes = {}
    for rel in sorted(set(filter(None, listed.split("\0")))):
        path = os.path.join(root, rel)
        if allow.in_scope(rel) and os.path.isfile(path):
            with open(path, "rb") as f:
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


def stale(sizes, allow):
    bad = []
    for rel, budget in sorted(allow.entries.items()):
        n = sizes.get(rel)
        if n is None:
            bad.append(f"{allow.path} lists {rel}, which is gone or not scanned")
        elif n <= allow.limit_of(rel):
            bad.append(f"{allow.path} lists {rel} at {budget}, it is under the limit ({n} lines)")
        elif n < budget:
            bad.append(f"{allow.path} lists {rel} at {budget}, it has {n} lines")
    return bad


def report(violations, hint=None):
    for v in violations:
        print(f"file-size: {v}", file=sys.stderr)
    if hint:
        print(f"file-size: {hint}", file=sys.stderr)
    return 1 if violations else 0


SPLIT_HINT = "a file over its budget is split into modules; its budget never goes up"


def cmd_check(root, allow):
    sizes = scanned(root, allow)
    grown = over_budget(sizes, allow)
    old = stale(sizes, allow)
    rc = report(grown, SPLIT_HINT if grown else None)
    return report(old, "run `file-size.py --update` and commit the allowlist" if old else None) or rc


def cmd_staged(root, allow_rel):
    staged = git(root, "diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR")
    allow_blob = subprocess.run(["git", "-C", root, "show", f":{allow_rel}"], capture_output=True)
    if allow_blob.returncode == 0:
        allow = Allow(allow_blob.stdout.decode(), allow_rel)
    else:
        allow = load_allow(root, allow_rel)
    sizes = {}
    for rel in filter(None, staged.split("\0")):
        if allow.in_scope(rel):
            sizes[rel] = count_lines(git(root, "cat-file", "blob", f":{rel}", binary=True))
    grown = over_budget(sizes, allow)
    return report(grown, SPLIT_HINT if grown else None)


def cmd_update(root, allow, allow_file):
    sizes = scanned(root, allow)
    out, changed = [], []
    for raw in allow.lines:
        words = raw.split("#", 1)[0].split()
        if len(words) == 2 and words[0] in allow.entries:
            rel, budget = words[0], int(words[1])
            n = sizes.get(rel)
            if n is None or n <= allow.limit_of(rel):
                changed.append(f"removed {rel} ({'gone' if n is None else f'{n} lines'})")
                continue
            if n < budget:
                changed.append(f"lowered {rel} {budget} -> {n}")
                note = raw.split("#", 1)[1] if "#" in raw else None
                raw = f"{rel} {n}" + (f"  #{note}" if note is not None else "")
        out.append(raw)
    if changed:
        with open(allow_file, "w") as f:
            f.write("\n".join(out) + "\n")
    for c in changed:
        print(f"file-size: {c}")
    allow = load_allow(root, os.path.relpath(allow_file, root))
    grown = over_budget(sizes, allow)
    return report(grown, SPLIT_HINT if grown else None)


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
    path = os.path.join(root, allow_rel)
    if not os.path.isfile(path):
        fail_setup(f"no allowlist at {path}")
    with open(path) as f:
        return Allow(f.read(), allow_rel)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("-C", dest="root", default=".", help="any directory of the repository")
    p.add_argument("--allow", default="file-size.allow", help="relative to the root")
    mode = p.add_mutually_exclusive_group()
    mode.add_argument("--staged", action="store_true")
    mode.add_argument("--update", action="store_true")
    mode.add_argument("--report", action="store_true")
    a = p.parse_args()
    root = git(a.root, "rev-parse", "--show-toplevel").strip()
    if a.staged:
        return cmd_staged(root, a.allow)
    allow = load_allow(root, a.allow)
    if a.update:
        return cmd_update(root, allow, os.path.join(root, a.allow))
    if a.report:
        return cmd_report(root, allow)
    return cmd_check(root, allow)


if __name__ == "__main__":
    sys.exit(main())
