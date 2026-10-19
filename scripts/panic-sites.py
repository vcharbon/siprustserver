#!/usr/bin/env python3
"""Panic-sites ratchet of the tiered crates (ADR-0051 §2-§3).

  panic-sites.py [-C DIR] [--allow FILE] [--base REV] [-p CRATE]... [--json FILE]
                 [--staged | --update | --report]

A tiered crate is a workspace member declaring `[package.metadata.purity]
tier = "pure" | "shell"`. One clippy pass runs over their `--lib --bins`
targets with the panic set force-warned (`--force-warn`), so no crate config,
no `-D warnings` gate and no source attribute changes what it sees, under
the root clippy.toml (`CLIPPY_CONF_DIR` is the root):

  clippy::indexing_slicing  string_slice  unwrap_used  expect_used  panic
  unreachable  todo  unimplemented  cast_possible_truncation  cast_sign_loss
  cast_possible_wrap, in pure crates also iter_over_hash_type, and the three
  lists of the root clippy.toml:
  disallowed_macros  disallowed_methods  disallowed_types

A list entry whose reason starts with `pure tier` counts in pure crates only;
every other entry counts in both tiers. A disallowed `assert!` expanded from
`debug_assert!` does not count: release compiles it out. A disallowed
`unwrap`/`expect` call an `unwrap_used`/`expect_used` site holds is that
one site. A site is exempt when one `#[expect(lint, reason = "…")]` on an
item or statement covers it and no other site of its lint.

The allowlist (`panic-sites.allow` at the root of DIR's repository) holds
`path count` entries: a file may have at most its count, an unlisted file
none. Against its version at a base (REV, by default the merge-base of HEAD
and `develop`, else HEAD; the same under --staged) the allowlist only
tightens. An entry above its base count (0 when the base does not list it;
a file git sees renamed keeps its old entry's count) passes only when the
allowlist's total does not rise and the excess is at most the file's sites
on lines git sees moved (panic_sites_diff). Moved sites carry budget only as
far as the files they moved out of were counted at the base and have fallen
since; past that every moved line counts as touched. The root clippy.toml's
lists only tighten too (panic_sites_lists).

A crate with no entry is at zero: each of its lib and bin roots carries

  #![cfg_attr(not(test), deny(<the eleven lints above>))]

(a pure crate's with iter_over_hash_type too), which exempts all test code.
The three disallowed lints stay with this pass,
whose tier split no crate attribute can state. Attributes that would hide a
site from a plain clippy run, and `cfg` predicates that keep code out of the
pass, are refused in a tiered crate (panic_sites_attrs).

  (no flag)  the pass, then: a file over its count, a counted site on a line
             the branch touches against the base (untracked files diffed as
             new; moved lines are not touched), the allowlist and the lists
             against the base, a refused attribute, a crate at zero without
             its attribute, a stale entry (above its file's count, or its
             file at zero or gone; `--update` fixes it)
  -p CRATE   the pass over these members only, the untiered among them
             skipped; entries of the other crates are left alone
  --json F   reads the pass's clippy JSON from F instead of running it
  --staged   the staged allowlist and clippy.toml against the base only (a
             pre-commit hook; moved sites are not known there, so a raised
             entry passes when the total does not rise)
  --update   lowers each entry to its file's count, moves the entry of a
             renamed file, removes the entries at zero or gone, and raises
             or adds an entry by the file's sites on moved lines
  --report   the counts per crate, then one `path count` line per file with a
             site, in allowlist form; exits 0

Exit 1 on a violation, 2 on a usage or setup error.
"""
import argparse
import json
import os
import subprocess
import sys

sys.dont_write_bytecode = True  # the helper modules beside it leave no __pycache__

from panic_sites_attrs import expects, refusals, zero_lints, zero_of, zero_text
from panic_sites_diff import lines as diff_lines, renames
from panic_sites_lints import COUNTED, PURE_COUNTED
from panic_sites_lists import entries as list_entries, loosened

PURE_ONLY = "pure tier"
DEBUG_ASSERTS = ("debug_assert!", "debug_assert_eq!", "debug_assert_ne!")


def fail_setup(msg):
    print(f"panic-sites: {msg}", file=sys.stderr)
    sys.exit(2)


def git(root, *args):
    out = subprocess.run(["git", "-C", root, *args], capture_output=True)
    if out.returncode != 0:
        fail_setup(f"git {' '.join(args)}: {out.stderr.decode(errors='replace').strip()}")
    return out.stdout.decode()


def show(root, spec):
    """The text of `spec` (`rev:path` or `:path`), or None."""
    out = subprocess.run(["git", "-C", root, "show", spec], capture_output=True)
    return out.stdout.decode() if out.returncode == 0 else None


def rev_exists(root, rev):
    out = subprocess.run(["git", "-C", root, "rev-parse", "--verify", "-q", f"{rev}^{{commit}}"],
                         capture_output=True)
    return out.returncode == 0


def default_base(root):
    if not rev_exists(root, "HEAD"):
        return None
    if rev_exists(root, "develop"):
        return git(root, "merge-base", "HEAD", "develop").strip()
    return "HEAD"


class Allow:
    def __init__(self, text, path):
        self.path = path
        self.lines = text.splitlines()
        self.entries = {}
        for n, raw in enumerate(self.lines, 1):
            words = raw.split("#", 1)[0].split()
            if not words:
                continue
            if len(words) != 2 or not words[1].isdigit():
                fail_setup(f"{path}:{n}: unreadable line: {raw.strip()}")
            if words[0] in self.entries:
                fail_setup(f"{path}:{n}: {words[0]} is listed twice")
            self.entries[words[0]] = int(words[1])

    def total(self):
        return sum(self.entries.values())


def load_allow(root, rel):
    path = os.path.join(root, rel)
    if not os.path.isfile(path):
        fail_setup(f"no allowlist at {path}")
    with open(path) as f:
        return Allow(f.read(), rel)


def base_allow(root, base, rel):
    """The allowlist at `base`, or None when there is none to hold it to."""
    if base is None or not rev_exists(root, base):
        return None
    text = show(root, f"{base}:{rel}")
    return None if text is None else Allow(text, f"{base}:{rel}")


def base_count(rel, allow, base, renamed):
    """What `base` lists for `rel`, through a rename git sees; 0 unlisted."""
    if rel in base.entries:
        return base.entries[rel]
    src = renamed.get(rel)
    if src in base.entries and src not in allow.entries:
        return base.entries[src]
    return 0


def against_base(allow, base, renamed, moved_sites=None):
    """What `allow` loosens against its `base` version. An entry above its
    base count passes when the total does not rise and, with `moved_sites`
    known, the excess is at most the file's sites on moved lines."""
    if base is None:
        return []
    bad, rises = [], allow.total() > base.total()
    for rel, n in sorted(allow.entries.items()):
        old = base_count(rel, allow, base, renamed)
        if n <= old:
            continue
        moved = None if moved_sites is None else moved_sites.get(rel, 0)
        if not rises and (moved is None or n - old <= moved):
            continue
        why = (f"the allowlist total rises from {base.total()} to {allow.total()}" if rises
               else f"{n - old} above it, {moved} on moved lines")
        verb = f"lists {rel} at {n}, above its {old} at" if old else f"adds {rel} at {n}, which"
        tail = "" if old else " does not list"
        bad.append(f"{allow.path} {verb} {base.path}{tail} ({why})")
    return bad


class Crate:
    def __init__(self, pkg, root):
        self.name = pkg["name"]
        self.id = pkg["id"]
        self.tier = ((pkg.get("metadata") or {}).get("purity") or {}).get("tier")
        self.dir = os.path.relpath(os.path.dirname(pkg["manifest_path"]), root)
        self.roots = sorted(
            os.path.relpath(t["src_path"], root) for t in pkg["targets"]
            if set(t["kind"]) & {"lib", "rlib", "bin", "proc-macro"})

    def owns(self, rel):
        return rel.startswith(self.dir + "/")


def tiered_crates(root):
    out = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1"],
                         cwd=root, capture_output=True)
    if out.returncode != 0:
        fail_setup(f"cargo metadata: {out.stderr.decode(errors='replace').strip()}")
    meta = json.loads(out.stdout)
    root = meta["workspace_root"]
    crates = [Crate(p, root) for p in meta["packages"]]
    for c in crates:
        if c.tier not in (None, "pure", "shell"):
            fail_setup(f"{c.name} declares tier `{c.tier}`; a tier is `pure` or `shell`")
    return root, [c for c in crates if c.tier], {c.name for c in crates}


def clippy_pass(root, crates):
    """The pass: the counted lints force-warned, so no source attribute
    hides a site, under the root's clippy.toml whatever the environment
    says."""
    args = ["cargo", "clippy", "--keep-going", "--lib", "--bins", "--message-format=json"]
    for c in crates:
        args += ["-p", c.name]
    args.append("--")
    for code in COUNTED:
        args += ["--force-warn", code]
    out = subprocess.run(args, cwd=root, capture_output=True,
                         env={**os.environ, "CLIPPY_CONF_DIR": root})
    return out.returncode, out.stdout.decode(errors="replace"), out.stderr.decode(errors="replace")


def in_debug_assert(span):
    exp = span.get("expansion")
    while exp:
        if exp.get("macro_decl_name") in DEBUG_ASSERTS:
            return True
        exp = exp["span"].get("expansion")
    return False


def call_site(span):
    """The span itself when it lies in the workspace, else the first call site
    of its macro expansion that does."""
    while os.path.isabs(span["file_name"]) and span.get("expansion"):
        span = span["expansion"]["span"]
    return span


def pure_only(msg):
    return any(ch.get("level") == "note" and ch.get("message", "").startswith(PURE_ONLY)
               for ch in msg.get("children", []))


class Finding:
    """A counted site: its file and the lines its span covers."""

    def __init__(self, crate, site, code, level, message, rendered):
        self.crate, self.code, self.level, self.rendered = crate, code, level, rendered
        self.message = message
        self.file, self.line, self.end = (
            site["file_name"], site["line_start"], site.get("line_end", site["line_start"]))
        self.span = ((self.line, site.get("column_start", 0)),
                     (self.end, site.get("column_end", 0)))

    def lines(self):
        return set(range(self.line, self.end + 1))

    def within(self, other):
        return self.file == other.file and other.span[0] <= self.span[0] and \
            self.span[1] <= other.span[1]


# The library calls the `unwrap_used` / `expect_used` lints see too, except
# inside a local macro's expansion: listed in clippy.toml for that case, one
# site where both lints see it, and none written in another crate's macro
# (those lints leave external macros alone too).
UNWRAPS = tuple(f"`std::{t}::{m}`" for t in ("option::Option", "result::Result")
                for m in ("unwrap", "expect", "unwrap_err", "expect_err"))
UNWRAP_LINTS = ("clippy::unwrap_used", "clippy::expect_used")


def deduplicated(found):
    """`found` without a disallowed `unwrap`/`expect` call an `unwrap_used` or
    `expect_used` site already holds."""
    lints = [f for f in found if f.code in UNWRAP_LINTS]
    return [f for f in found if not (
        f.code == "clippy::disallowed_methods" and any(u in f.message for u in UNWRAPS)
        and any(f.within(g) for g in lints))]


def findings_of(text, crates):
    """The counted findings of clippy's JSON `text` and its other errors."""
    by_id = {c.id: c for c in crates}
    found, seen, errors = [], set(), []
    for raw in text.splitlines():
        if not raw.startswith("{"):
            continue
        rec = json.loads(raw)
        if rec.get("reason") != "compiler-message":
            continue
        msg = rec["message"]
        code = (msg.get("code") or {}).get("code")
        crate = by_id.get(rec.get("package_id"))
        primary = next((s for s in msg.get("spans", []) if s.get("is_primary")), None)
        if code not in COUNTED or crate is None or primary is None:
            if msg.get("level") == "error" or (
                    primary is not None and primary["file_name"].endswith("clippy.toml")):
                errors.append(msg.get("rendered") or msg.get("message", ""))
            continue
        if code == "clippy::disallowed_macros" and in_debug_assert(primary):
            continue
        if code == "clippy::disallowed_methods" and os.path.isabs(primary["file_name"]) and \
                any(u in msg["message"] for u in UNWRAPS):
            continue
        if crate.tier != "pure" and (pure_only(msg) or code in PURE_COUNTED):
            continue
        site = call_site(primary)
        key = (site["file_name"], site["line_start"], site["column_start"], code, msg["message"])
        if key in seen:
            continue
        seen.add(key)
        found.append(Finding(crate, site, code, msg.get("level"), msg["message"],
                             msg.get("rendered") or msg["message"]))
    return deduplicated(found), errors


def expected(root, found):
    """`found` without each site one reasoned `#[expect]` covers alone: an
    expect whose item holds two sites of its lint exempts neither."""
    exempt = set()
    for rel in sorted({f.file for f in found}):
        path = os.path.join(root, rel)
        if not os.path.isfile(path):
            continue
        mine = [f for f in found if f.file == rel]
        for lints, first, last in expects(read(root, rel)):
            for lint in lints:
                covered = [f for f in mine if f.code == lint and first <= f.line
                           and f.end <= last]
                if len(covered) == 1:
                    exempt.add(id(covered[0]))
    return [f for f in found if id(f) not in exempt]


def counts_of(found):
    counts = {}
    for f in found:
        counts[f.file] = counts.get(f.file, 0) + 1
    return counts


def moved_sites_of(found, moved):
    """`{file: sites}` of the sites whose every line git sees moved."""
    sites = {}
    for f in found:
        if f.lines() <= moved.get(f.file, set()):
            sites[f.file] = sites.get(f.file, 0) + 1
    return sites


def carried(found, counts, base, touched, moved, moved_from):
    """`(touched, moved sites)`: sites on moved lines carry budget only as far
    as the files lines moved out of were counted at the base and have fallen
    since. Past that, no moved site carries budget and every moved line counts
    as touched: git does not say which removed line an added one came from."""
    sites = moved_sites_of(found, moved)
    pool = 0 if base is None else sum(max(0, base.entries.get(rel, 0) - counts.get(rel, 0))
                                      for rel in moved_from)
    if sum(sites.values()) <= pool:
        return touched, sites
    return {f: touched.get(f, set()) | moved.get(f, set()) for f in set(touched) | set(moved)}, {}


def over_budget(found, counts, allow):
    bad = []
    for rel, n in sorted(counts.items()):
        budget = allow.entries.get(rel, 0)
        if n > budget:
            listed = "" if rel in allow.entries else " (unlisted)"
            bad.append(f"{rel} has {n} panic-class sites, budget {budget}{listed}")
            if rel not in allow.entries:
                bad.extend(f"  {f.file}:{f.line} {f.code}" for f in found if f.file == rel)
    return bad


def in_hunks(found, touched):
    """The sites whose span holds a line the branch touches."""
    return [f"new site in a touched hunk:\n{f.rendered.rstrip()}" for f in found
            if f.lines() & touched.get(f.file, set())]


def stale(counts, allow, crates):
    bad = []
    for rel, budget in sorted(allow.entries.items()):
        if not any(c.owns(rel) for c in crates):
            continue
        n = counts.get(rel, 0)
        if n < budget:
            bad.append(f"{allow.path} lists {rel} at {budget}, it has {n}")
    return bad


def unowned(allow, every):
    return [f"{allow.path} lists {rel}, which no tiered crate holds"
            for rel in sorted(allow.entries) if not any(c.owns(rel) for c in every)]


def read(root, rel):
    with open(os.path.join(root, rel), encoding="utf-8", errors="replace") as f:
        return f.read()


def zero_attrs(root, crates, allow, counts):
    """A crate with no entry and no site carries the zero attribute at each
    root; a crate with entries carries none."""
    bad = []
    for c in crates:
        listed = any(c.owns(rel) for rel in allow.entries)
        at_zero = not listed and not any(c.owns(rel) for rel in counts)
        for rel in c.roots:
            have, want = zero_lints(read(root, rel)), zero_of(c.tier)
            if at_zero and (have is None or not want <= have):
                miss = "" if have is None else f" (missing {', '.join(sorted(want - have))})"
                bad.append(f"{c.name} is at zero: {rel} carries `{zero_text(c.tier)}`{miss}")
            elif listed and have is not None:
                bad.append(f"{c.name} has allowlist entries: {rel} carries the zero attribute")
    return bad


TEST_DIRS = ("tests", "benches", "examples")


def lint_attrs(root, crates):
    """The refused attributes of the tiered crates' sources the pass reads
    (their tests, benches and examples aside)."""
    bad = []
    for c in crates:
        listed = git(root, "ls-files", "-z", "--cached", "--others", "--exclude-standard",
                     "--", c.dir)
        for rel in sorted(filter(None, listed.split("\0"))):
            if os.path.relpath(rel, c.dir).split(os.sep)[0] in TEST_DIRS:
                continue
            if rel.endswith(".rs") and os.path.isfile(os.path.join(root, rel)):
                bad += [f"{rel}:{line}: {why}" for line, why in refusals(read(root, rel))]
    return bad


def lists_against_base(root, base, staged):
    """What the root clippy.toml's lists loosen against `base` (HEAD and the
    index under `staged`)."""
    if base is None or not rev_exists(root, base):
        return []
    then = show(root, f"{base}:clippy.toml")
    if then is None:
        return []
    now = show(root, ":clippy.toml") if staged else None
    if now is None:
        path = os.path.join(root, "clippy.toml")
        if not os.path.isfile(path):
            return [f"clippy.toml is gone; {base}:clippy.toml holds the lists"]
        now = read(root, "clippy.toml")
    try:
        return loosened(list_entries(now, "clippy.toml"), list_entries(then, base), "clippy.toml",
                        f"{base}:clippy.toml")
    except ValueError as e:
        fail_setup(str(e))


def report(violations, hint=None):
    for v in violations:
        print(f"panic-sites: {v}", file=sys.stderr)
    if violations and hint:
        print(f"panic-sites: {hint}", file=sys.stderr)
    return 1 if violations else 0


GROW_HINT = ("a panic-class site is rewritten (checked access, an error returned), never "
             "listed; a count never goes up (ADR-0051)")
BASE_HINT = "the allowlist only tightens: an entry rises only by the sites moved into its file"
LIST_HINT = "the lists only tighten: a pass that sees fewer sites lowers every count"


def run_pass(root, crates, json_file):
    if json_file:
        with open(json_file) as f:
            text, rc, stderr = f.read(), 0, ""
    else:
        rc, text, stderr = clippy_pass(root, crates)
    found, errors = findings_of(text, crates)
    found = expected(root, found)
    denied = [f for f in found if f.level == "error"]
    if errors or denied or rc != 0:
        for e in errors + [f.rendered for f in denied]:
            print(e.rstrip(), file=sys.stderr)
        if not errors and not denied:
            print(stderr.rstrip(), file=sys.stderr)
        print(f"panic-sites: the clippy pass failed (exit {rc}): {len(denied)} site(s) at deny, "
              f"{len(errors)} other error(s); the crates after them went unlinted",
              file=sys.stderr)
        return found, False
    return found, True


def diff_of(root, base):
    try:
        return diff_lines(root, base), renames(root, base, False)
    except RuntimeError as e:
        fail_setup(str(e))


def cmd_check(root, allow, base, crates, every, json_file):
    found, ok = run_pass(root, crates, json_file)
    counts = counts_of(found)
    (touched, moved, moved_from), renamed = diff_of(root, base)
    base_list = base_allow(root, base, allow.path)
    touched, moved_sites = carried(found, counts, base_list, touched, moved, moved_from)
    rc = 0 if ok else 1
    rc = report(over_budget(found, counts, allow), GROW_HINT) or rc
    rc = report(in_hunks(found, touched), GROW_HINT) or rc
    rc = report(against_base(allow, base_list, renamed, moved_sites), BASE_HINT) or rc
    rc = report(lists_against_base(root, base, False), LIST_HINT) or rc
    rc = report(lint_attrs(root, crates)) or rc
    if not ok:
        return rc
    rc = report(zero_attrs(root, crates, allow, counts)) or rc
    old = stale(counts, allow, crates) + unowned(allow, every)
    return report(old, "run `panic-sites.py --update` and commit the allowlist") or rc


def cmd_staged(root, rel):
    """The staged allowlist and lists against the base verify holds them to,
    so a move committed in steps passes each one."""
    text = show(root, f":{rel}")
    allow = Allow(text, rel) if text is not None else load_allow(root, rel)
    base = default_base(root)
    try:
        renamed = renames(root, base, True)
    except RuntimeError as e:
        fail_setup(str(e))
    rc = report(against_base(allow, base_allow(root, base, rel), renamed), BASE_HINT)
    return report(lists_against_base(root, base, True), LIST_HINT) or rc


def updated(allow, counts, moved_sites, base, renamed, crates):
    """`(entries, changes)`: each entry lowered to its file's count or
    removed at zero, a renamed file's moved, and raised or added by the
    file's sites on moved lines."""
    moved_to = {old: new for new, old in renamed.items()}
    entries, notes, changes = {}, {}, []
    for raw in allow.lines:
        words = raw.split("#", 1)[0].split()
        if len(words) != 2:
            continue
        rel, budget = words[0], int(words[1])
        notes[rel] = raw.split("#", 1)[1] if "#" in raw else None
        new = moved_to.get(rel)
        if new and new not in allow.entries and new in counts | moved_sites:
            changes.append(f"moved {rel} -> {new}")
            notes[new] = notes.pop(rel)
            rel = new
        entries[rel] = budget
    for rel in sorted(set(entries) | set(counts)):
        if not any(c.owns(rel) for c in crates):
            continue
        n, budget = counts.get(rel, 0), entries.get(rel, 0)
        old = 0 if base is None else base_count(rel, allow, base, renamed)
        ceiling = max(budget, old + moved_sites.get(rel, 0))
        want = min(n, ceiling)
        if want == budget:
            continue
        verb = ("removed" if want == 0 else "lowered" if want < budget else
                "raised" if budget else "added")
        changes.append(f"{verb} {rel} {budget} -> {want}")
        if want == 0:
            entries.pop(rel, None)
        else:
            entries[rel] = want
    return entries, notes, changes


def cmd_update(root, allow, allow_file, base, crates, json_file):
    found, ok = run_pass(root, crates, json_file)
    if not ok:
        return 1
    counts = counts_of(found)
    (touched, moved, moved_from), renamed = diff_of(root, base)
    base_list = base_allow(root, base, allow.path)
    _, moved_sites = carried(found, counts, base_list, touched, moved, moved_from)
    entries, notes, changes = updated(allow, counts, moved_sites, base_list, renamed, crates)
    if changes:
        first = next((i for i, raw in enumerate(allow.lines)
                      if len(raw.split("#", 1)[0].split()) == 2), len(allow.lines))
        body = [f"{rel} {n}" + (f"  #{notes[rel]}" if notes.get(rel) is not None else "")
                for rel, n in sorted(entries.items())]
        with open(allow_file, "w") as f:
            f.write("\n".join(allow.lines[:first] + body) + "\n")
    for c in changes:
        print(f"panic-sites: {c}")
    allow = load_allow(root, allow.path)
    rc = report(over_budget(found, counts, allow), GROW_HINT)
    return report(against_base(allow, base_list, renamed, moved_sites), BASE_HINT) or rc


def cmd_report(root, allow, crates, json_file):
    found, _ = run_pass(root, crates, json_file)
    counts = counts_of(found)
    for c in sorted(crates, key=lambda c: c.name):
        files = {rel: n for rel, n in counts.items() if c.owns(rel)}
        state = "at zero" if not files else f"{sum(files.values())} sites in {len(files)} files"
        print(f"# {c.name} ({c.tier}): {state}")
    for rel, n in sorted(counts.items()):
        print(f"{rel} {n}")
    print(f"panic-sites: {sum(counts.values())} sites in {len(counts)} files; "
          f"{len(allow.entries)} allowlist entries", file=sys.stderr)
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("-C", dest="root", default=".", help="any directory of the repository")
    p.add_argument("--allow", default="panic-sites.allow", help="relative to the root")
    p.add_argument("--base", help="the revision the allowlist is held to (not with --staged)")
    p.add_argument("-p", dest="crates", action="append", default=[], metavar="CRATE")
    p.add_argument("--json", dest="json_file", help="the pass's clippy JSON, recorded")
    mode = p.add_mutually_exclusive_group()
    mode.add_argument("--staged", action="store_true")
    mode.add_argument("--update", action="store_true")
    mode.add_argument("--report", action="store_true")
    a = p.parse_args()
    root = git(a.root, "rev-parse", "--show-toplevel").strip()
    if a.staged:
        if a.base:
            fail_setup("--staged holds the allowlist to HEAD; --base does not apply")
        return cmd_staged(root, a.allow)
    if a.base and not rev_exists(root, a.base):
        fail_setup(f"--base {a.base} is no commit")
    base = a.base or default_base(root)
    allow = load_allow(root, a.allow)
    ws_root, every, members = tiered_crates(root)
    if os.path.realpath(ws_root) != os.path.realpath(root):
        fail_setup(f"the workspace root {ws_root} is not the repository root {root}")
    if not every:
        fail_setup("no workspace member declares [package.metadata.purity] tier")
    crates = every
    if a.crates:
        unknown = sorted(set(a.crates) - members)
        if unknown:
            fail_setup(f"not a workspace member: {', '.join(unknown)}")
        crates = [c for c in every if c.name in a.crates]
        if not crates:
            print("panic-sites: no tiered crate among the named ones", file=sys.stderr)
            return 0
    if a.update:
        return cmd_update(root, allow, os.path.join(root, a.allow), base, crates, a.json_file)
    if a.report:
        return cmd_report(root, allow, crates, a.json_file)
    return cmd_check(root, allow, base, crates, every, a.json_file)


if __name__ == "__main__":
    sys.exit(main())
