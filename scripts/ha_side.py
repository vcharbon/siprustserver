"""The blind/aware boundary and the word net, as purity.py runs them
(ADR-0055 §11).

Every member declares the side it is on:

  [package.metadata.ha]
  side = "blind"                 # or "aware"

A blind crate holds single-node logic only. Its normal dependencies, optional
ones included, are first-party crates that are blind too (a third-party crate
holds no side), and none of its tracked files says `takeover`, `reclaim`,
`tombstone`, `materialise`/`materialize` or `stand-down` in any spelling
(`stand_down`, `StandDown`, `stand down`), nor `replica` as a word of its own
(an identifier is split at `_`, digits and case changes, so `peek_replica` and
`ReplicaSet` hold it and `replicated` does not). There is no allowlist: a hit
is moved to an aware crate or reworded to the single-node statement it makes.
"""
import os
import re
import subprocess

SIDES = ("blind", "aware")
STEMS = re.compile(r"takeover|reclaim|tombstone|materiali[sz]|stand[-_ ]?down", re.IGNORECASE)
WORDS = {"replica", "replicas"}
RUN = re.compile(r"[A-Za-z]+")
PART = re.compile(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+")


def side_of(pkg):
    return ((pkg.get("metadata") or {}).get("ha") or {}).get("side")


def hits(line):
    """The refused words `line` holds, in order."""
    found = [m.group(0) for m in STEMS.finditer(line)]
    for run in RUN.findall(line):
        found += [p for p in PART.findall(run) if p.lower() in WORDS]
    return found


def tracked(root, crate_dir):
    """The files of `crate_dir` git tracks or would track, relative to `root`."""
    out = subprocess.run(["git", "-C", root, "ls-files", "-z", "--cached", "--others",
                          "--exclude-standard", "--", os.path.relpath(crate_dir, root)],
                         capture_output=True, check=True)
    return sorted(set(filter(None, out.stdout.decode().split("\0"))))


def word_net(root, pkg):
    """Each refused word of `pkg`'s files, as `file:line holds "word"`."""
    bad = []
    crate_dir = os.path.dirname(pkg["manifest_path"])
    for rel in tracked(root, crate_dir):
        path = os.path.join(root, rel)
        if not os.path.isfile(path):
            continue
        try:
            with open(path, encoding="utf-8") as f:
                lines = f.read().splitlines()
        except UnicodeDecodeError:
            continue
        for n, line in enumerate(lines, 1):
            for word in hits(line):
                bad.append(f'blind crate {pkg["name"]}: {rel}:{n} holds "{word}"; move it to '
                           "an aware crate or state it for a single node (ADR-0055 §11)")
    return bad


def violations(root, members, packages):
    """The boundary over `members`, the sides read off every first-party
    package in `packages`."""
    sides = {p["name"]: side_of(p) for p in packages}
    bad = []
    for pkg in sorted(members, key=lambda p: p["name"]):
        side = sides.get(pkg["name"])
        if side is None:
            bad.append(f'{pkg["name"]} declares no HA side: [package.metadata.ha] '
                       'side = "blind" | "aware"')
            continue
        if side not in SIDES:
            bad.append(f'{pkg["name"]} declares HA side `{side}`; a side is `blind` or `aware`')
            continue
        if side != "blind":
            continue
        for dep in sorted({d["name"] for d in pkg["dependencies"] if d.get("kind") is None}):
            if dep in sides and sides[dep] != "blind":
                what = f"declares side {sides[dep]}" if sides[dep] else "declares no HA side"
                bad.append(f"blind crate {pkg['name']} depends on {dep}, which {what}")
        bad += word_net(root, pkg)
    return bad
