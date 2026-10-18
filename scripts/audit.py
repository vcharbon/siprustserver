#!/usr/bin/env python3
"""RustSec audit of a workspace's Cargo.lock: a gate on the branches that change it.

  audit.py [-C DIR] [--base REV] [--db PATH] [--report [--outdated TOOL]]

`cargo audit` reads the lockfile at the root of DIR's repository and its
configuration `.cargo/audit.toml` there, whose `[advisories] ignore` lists the
accepted advisories, each with its reason.

  (no flag)  one summary line, then one line per vulnerability and per
             warning (unmaintained, unsound, yanked). When the branch touches
             `Cargo.lock` or `.cargo/audit.toml` against a base (REV, by
             default the merge-base of HEAD and `develop`, else HEAD; the
             worktree counts, untracked files included), a vulnerability not
             ignored fails. Otherwise the summary is a warning and nothing
             fails, an absent cargo-audit included.
  --report   `cargo audit` in its own format, then the outdated report of the
             direct dependencies (TOOL: `cargo-upgrade`, the default, runs
             `cargo upgrade --dry-run --incompatible allow`, which reads
             `[workspace.dependencies]` and a workspace that path-depends on
             another one; `cargo-outdated` runs `cargo outdated --workspace
             --root-deps-only`, which reads neither). It prints; it exits 0
             whatever it found.

The advisory database (PATH, by default `$CARGO_HOME/advisory-db`) is fetched
at most once a day: younger than that, or when the fetch fails (an offline
box), the audit runs on the local copy, however old (`--no-fetch --stale`).

Exit 1 on a failing gate, 2 on a usage or setup error (a gated run without
cargo-audit included).
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time

GATED = ("Cargo.lock", ".cargo/audit.toml")
FETCH_EVERY_S = 24 * 3600
WARNING_KINDS = ("unmaintained", "unsound", "yanked")
OUTDATED = {
    "cargo-outdated": ["cargo", "outdated", "--workspace", "--root-deps-only"],
    "cargo-upgrade": ["cargo", "upgrade", "--dry-run", "--incompatible", "allow"],
}
INSTALL = "cargo-audit is not installed (pacman -S cargo-audit, or cargo install cargo-audit)"


def fail_setup(msg):
    print(f"audit: {msg}", file=sys.stderr)
    sys.exit(2)


def git(root, *args):
    out = subprocess.run(["git", "-C", root, *args], capture_output=True, text=True)
    if out.returncode != 0:
        fail_setup(f"git {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout


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


def touched(root, base):
    """The gated files the worktree changes against `base` (all of them, with no base)."""
    if base is None:
        return [p for p in GATED if os.path.exists(os.path.join(root, p))]
    changed = git(root, "diff", "--name-only", base, "--", *GATED).split()
    changed += git(root, "ls-files", "--others", "--exclude-standard", "--", *GATED).split()
    return sorted(set(changed))


def default_db():
    home = os.environ.get("CARGO_HOME") or os.path.join(os.path.expanduser("~"), ".cargo")
    return os.path.join(home, "advisory-db")


def db_age_s(db):
    """Seconds since the database was last fetched, None when it never was."""
    for stamp in (os.path.join(db, ".git", "FETCH_HEAD"), os.path.join(db, ".git", "HEAD")):
        if os.path.exists(stamp):
            return max(0.0, time.time() - os.path.getmtime(stamp))
    return None


def age_text(seconds):
    if seconds is None:
        return "never fetched"
    hours = int(seconds // 3600)
    return f"fetched {hours // 24} d ago" if hours >= 48 else f"fetched {hours} h ago"


def run_audit(root, db, extra):
    """`cargo audit` with `extra`, fetching the database when it is a day old or more.

    Returns the completed process and the database's age line."""
    base = ["cargo", "audit", "--db", db, *extra]
    age = db_age_s(db)
    if age is None or age >= FETCH_EVERY_S:
        out = subprocess.run(base, cwd=root, capture_output=True, text=True)
        if fetched(out):
            return out, age_text(db_age_s(db))
        note = "fetch failed, local copy used"
    else:
        note = "local copy"
    out = subprocess.run([*base, "--no-fetch", "--stale"], cwd=root, capture_output=True, text=True)
    return out, f"{age_text(age)}, {note}"


def fetched(out):
    """Whether a run that fetched reached the audit: exit 0 or 1 is a report."""
    return out.returncode in (0, 1) and "error: couldn't fetch" not in out.stderr


def summarize(report):
    """(vulnerabilities, warnings) of a `cargo audit --json` report, each a list of lines."""
    vulns = []
    for v in report.get("vulnerabilities", {}).get("list", []):
        adv, pkg = v.get("advisory") or {}, v.get("package") or {}
        patched = ", ".join((v.get("versions") or {}).get("patched") or []) or "no fixed version"
        vulns.append(f"{pkg.get('name')} {pkg.get('version')} {adv.get('id')}: "
                     f"{adv.get('title')} (fixed in {patched})")
    warns = []
    for kind in WARNING_KINDS:
        for w in report.get("warnings", {}).get(kind, []):
            adv, pkg = w.get("advisory") or {}, w.get("package") or {}
            warns.append(f"{kind} {pkg.get('name')} {pkg.get('version')} {adv.get('id', '')}".rstrip())
    return vulns, warns


def cmd_gate(root, base, db):
    changed = touched(root, base)
    gated = bool(changed)
    if shutil.which("cargo-audit") is None:
        if gated:
            fail_setup(f"{INSTALL}; the branch touches {', '.join(changed)}")
        print(f"audit: warning: {INSTALL}; nothing audited")
        return 0
    out, age = run_audit(root, db, ["--json"])
    try:
        report = json.loads(out.stdout)
    except json.JSONDecodeError:
        last = (out.stderr.strip().splitlines() or ["no output"])[0]
        if gated:
            fail_setup(f"cargo audit gave no report (exit {out.returncode}): {last}")
        print(f"audit: warning: cargo audit gave no report (exit {out.returncode}): {last}")
        return 0
    vulns, warns = summarize(report)
    why = (f"the branch touches {', '.join(changed)}" if gated
           else "the branch touches neither Cargo.lock nor .cargo/audit.toml")
    head = (f"{len(vulns)} vulnerabilit{'y' if len(vulns) == 1 else 'ies'}, "
            f"{len(warns)} warning(s) in Cargo.lock (advisory db {age})")
    failing = gated and bool(vulns)
    if failing:
        print(f"audit: FAIL: {head}; {why}")
    elif vulns:
        print(f"audit: warning: {head}; {why}, so they do not fail")
    else:
        print(f"audit: ok: {head}")
    for line in vulns:
        print(f"audit: vulnerable {line}")
    for line in warns:
        print(f"audit: {line}")
    if failing:
        print("audit: fix it (cargo update -p <crate>, a bump) or accept it in "
              ".cargo/audit.toml [advisories] ignore, with its reason")
    return 1 if failing else 0


def cmd_report(root, db, outdated):
    if shutil.which("cargo-audit") is None:
        print(f"audit: {INSTALL}")
    else:
        out, age = run_audit(root, db, [])
        print(f"== cargo audit (advisory db {age})")
        sys.stdout.write(out.stdout)
        sys.stdout.write(out.stderr)
    cmd = OUTDATED[outdated]
    print(f"== {' '.join(cmd)}")
    sys.stdout.flush()
    if shutil.which(outdated) is None:
        print(f"audit: {outdated} is not installed")
        return 0
    subprocess.run(cmd, cwd=root)
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("-C", dest="root", default=".", help="any directory of the repository")
    p.add_argument("--base", help="the revision the gated files are compared with")
    p.add_argument("--db", default=default_db(), help="the advisory database")
    p.add_argument("--report", action="store_true")
    p.add_argument("--outdated", choices=sorted(OUTDATED), default="cargo-upgrade")
    a = p.parse_args()
    root = git(a.root, "rev-parse", "--show-toplevel").strip()
    if not os.path.isfile(os.path.join(root, "Cargo.lock")):
        fail_setup(f"no Cargo.lock at {root}")
    if a.report:
        return cmd_report(root, a.db, a.outdated)
    if a.base and not rev_exists(root, a.base):
        fail_setup(f"--base {a.base} is no commit")
    return cmd_gate(root, a.base or default_base(root), a.db)


if __name__ == "__main__":
    sys.exit(main())
