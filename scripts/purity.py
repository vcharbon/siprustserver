#!/usr/bin/env python3
"""Tiers, production closures and the pure dependency rule (ADR-0051 §1, §3).

  purity.py [-C DIR] [--config FILE] [--base REV] [--staged]

A crate declares its tier in its manifest:

  [package.metadata.purity]
  tier = "pure"                  # or "shell"

The configuration (`purity.toml` at the root of DIR's repository):

  binaries = ["a-runner"]        the production binaries
  test-features = ["testkit", "a-clock/sim"]
                                 features no binary may reach: a bare name
                                 is that feature of any crate
  pure-deps = ["serde", ...]     third-party crates a pure crate may reach
  [known]                        breaches held until their fix lands
  untiered = ["a-harness"]       closure crates without a tier
  [known.pure-reaches]
  a-pure = ["a-shell"]           what a pure crate reaches against the rule

Checks:

  closure    each binary's `cargo tree -p <binary> -e normal,features`, its
             resolution when built by a cargo command of its own: a test
             feature fails, and so does a first-party crate without a tier
  pure       over each pure member's `cargo tree -p <crate> -e
             normal,no-proc-macro` and each binary's (the features the
             binary turns on): a first-party crate a pure crate depends on
             is pure, a third-party crate it reaches is in `pure-deps`; a
             `pure-deps` entry no pure crate reaches is stale
  lints      every member inherits `[lints] workspace = true` or restates
             the workspace's table, `rust.unsafe_code = "deny"` its one
             allowed difference; no clippy.toml below the root and no
             `.clippy.toml` at all (clippy reads the nearest one and does not
             merge); no `.cargo/config` that caps lints, lowers one in
             rustflags or sets CLIPPY_CONF_DIR
  base       against the base (REV, by default the merge-base of HEAD and
             `develop`, else HEAD): no known breach added, no binary or test
             feature removed; a known entry whose breach is gone is stale

  --staged   the staged configuration against the same base only (a
             pre-commit hook)

Exit 1 on a violation, 2 on a usage or setup error.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tomllib

TIERS = ("pure", "shell")
LINE = re.compile(r"^(\d+)(\S+) (?:v(\S+)(?: \(([^)]*)\))?|feature \"([^\"]+)\")")


def fail_setup(msg):
    print(f"purity: {msg}", file=sys.stderr)
    sys.exit(2)


def git(root, *args):
    out = subprocess.run(["git", "-C", root, *args], capture_output=True)
    if out.returncode != 0:
        fail_setup(f"git {' '.join(args)}: {out.stderr.decode(errors='replace').strip()}")
    return out.stdout.decode()


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


class Config:
    def __init__(self, text, path):
        try:
            data = tomllib.loads(text)
        except tomllib.TOMLDecodeError as e:
            fail_setup(f"{path}: {e}")
        unknown = set(data) - {"binaries", "test-features", "pure-deps", "known"}
        if unknown:
            fail_setup(f"{path}: unknown key(s) {', '.join(sorted(unknown))}")
        self.path = path
        self.binaries = data.get("binaries", [])
        self.test_features = set(data.get("test-features", []))
        self.pure_deps = set(data.get("pure-deps", []))
        known = data.get("known", {})
        if set(known) - {"untiered", "pure-reaches"}:
            fail_setup(f"{path}: [known] holds `untiered` and `pure-reaches` only")
        self.untiered = set(known.get("untiered", []))
        self.reaches = {(c, d) for c, ds in known.get("pure-reaches", {}).items() for d in ds}

    def known(self):
        return {("untiered", c) for c in self.untiered} | {
            ("pure-reaches", f"{c} -> {d}") for c, d in self.reaches}

    def is_test_feature(self, crate, feature):
        return feature in self.test_features or f"{crate}/{feature}" in self.test_features


def load_config(root, rel):
    path = os.path.join(root, rel)
    if not os.path.isfile(path):
        fail_setup(f"no configuration at {path}")
    with open(path, "rb") as f:
        return Config(f.read().decode(), rel)


def base_config(root, base, rel):
    if base is None or not rev_exists(root, base):
        return None
    out = subprocess.run(["git", "-C", root, "show", f"{base}:{rel}"], capture_output=True)
    return Config(out.stdout.decode(), f"{base}:{rel}") if out.returncode == 0 else None


def cargo(root, *args):
    out = subprocess.run(["cargo", *args], cwd=root, capture_output=True)
    if out.returncode != 0:
        fail_setup(f"cargo {' '.join(args)}: {out.stderr.decode(errors='replace').strip()}")
    return out.stdout.decode()


def tier_of(pkg):
    return ((pkg.get("metadata") or {}).get("purity") or {}).get("tier")


class Workspace:
    def __init__(self, root):
        meta = json.loads(cargo(root, "metadata", "--format-version", "1"))
        self.root = meta["workspace_root"]
        members = set(meta["workspace_members"])
        local = [p for p in meta["packages"] if p.get("source") is None]
        self.tiers = {p["name"]: tier_of(p) for p in local}
        self.members = [p for p in local if p["id"] in members]
        self.local_names = set(self.tiers)

    def bad_tiers(self):
        return [f"{n} declares tier `{t}`; a tier is `pure` or `shell`"
                for n, t in sorted(self.tiers.items()) if t is not None and t not in TIERS]


def tree(root, *args):
    """`(depth, name, is_local, feature)` per line of a depth-prefixed tree."""
    rows = []
    for line in cargo(root, "tree", *args, "--prefix", "depth", "-f", "{p}").splitlines():
        m = LINE.match(line)
        if not m:
            continue
        depth, name, _ver, paren, feature = m.groups()
        rows.append((int(depth), name, bool(paren and paren.startswith("/")), feature))
    return rows


def via(chain):
    """A tree path as its crate names and test features; another feature node
    is `None`."""
    names = [n for n in chain if n is not None]
    return " > ".join(n for i, n in enumerate(names) if i == 0 or names[i - 1] != n)


def closure(ws, config):
    bad, untiered = [], set()
    for binary in config.binaries:
        if binary not in ws.local_names:
            bad.append(f"{config.path} names binary {binary}, which is no first-party crate")
            continue
        chain = []
        for depth, name, local, feature in tree(ws.root, "-p", binary, "-e", "normal,features"):
            test = feature is not None and config.is_test_feature(name, feature)
            del chain[depth:]
            chain.append(name if feature is None else f"{name}/{feature}" if test else None)
            if test:
                bad.append(f"{binary} reaches {name} feature \"{feature}\" "
                           f"(via {via(chain[:-1])})")
            elif feature is None and local and ws.tiers.get(name) is None:
                untiered.add(name)
                if name not in config.untiered:
                    bad.append(f"{binary} links {name}, which declares no tier "
                               f"(via {via(chain[:-1]) or binary})")
    for name in sorted(config.untiered - untiered):
        bad.append(f"{config.path} lists {name} as untiered, which no binary links or it "
                   "declares a tier: remove the entry")
    return bad


class Graph:
    """The normal dependency edges of several resolved trees, by crate name."""

    def __init__(self):
        self.children, self.local = {}, set()

    def add(self, rows):
        stack = []
        for depth, name, local, _ in rows:
            del stack[depth:]
            self.children.setdefault(name, set())
            if local:
                self.local.add(name)
            if stack:
                self.children[stack[-1]].add(name)
            stack.append(name)


def dependency_graph(ws, config):
    graph = Graph()
    roots = [p["name"] for p in ws.members if ws.tiers.get(p["name"]) == "pure"]
    roots += [b for b in config.binaries if b in ws.local_names]
    for crate in roots:
        graph.add(tree(ws.root, "-p", crate, "-e", "normal,no-proc-macro"))
    return graph


def pure_rule(ws, config, graph):
    """What each pure crate reaches against the rule, `{(crate, dep): via}`,
    and the `pure-deps` entries some pure crate reaches."""
    breaches, used = {}, set()
    for crate in sorted(n for n in graph.local if ws.tiers.get(n) == "pure"):
        todo = [(c, [crate, c]) for c in sorted(graph.children.get(crate, ()))]
        seen = set()
        while todo:
            name, path = todo.pop()
            if name in seen:
                continue
            seen.add(name)
            if name in graph.local:
                if ws.tiers.get(name) != "pure":
                    breaches.setdefault((crate, name), " > ".join(path))
            elif name in config.pure_deps:
                used.add(name)
                todo += [(c, path + [c]) for c in sorted(graph.children.get(name, ()))]
            else:
                breaches.setdefault((crate, name), " > ".join(path))
    return breaches, used


def pure_violations(ws, config):
    breaches, used = pure_rule(ws, config, dependency_graph(ws, config))
    bad = []
    for (crate, dep), path in sorted(breaches.items()):
        if (crate, dep) in config.reaches:
            continue
        what = (f"declares tier {ws.tiers.get(dep)}" if ws.tiers.get(dep) else "declares no tier") \
            if dep in ws.local_names else "is not in pure-deps"
        bad.append(f"pure crate {crate} reaches {dep}, which {what} ({path})")
    for crate, dep in sorted(config.reaches - set(breaches)):
        bad.append(f"{config.path} lists {crate} -> {dep} under [known.pure-reaches], which no "
                   "longer holds: remove the entry")
    for dep in sorted(config.pure_deps - used):
        bad.append(f"{config.path} lists {dep} in pure-deps, which no pure crate reaches: "
                   "remove the entry")
    return bad


def restated(lints):
    """A crate's own lint table as the workspace's it must equal: its one
    allowed difference, `rust.unsafe_code = "deny"`, undone."""
    table = {k: dict(v) if isinstance(v, dict) else v for k, v in lints.items()}
    if table.get("rust", {}).get("unsafe_code") == "deny":
        table["rust"]["unsafe_code"] = "forbid"
    return table


def lint_tables(ws):
    with open(os.path.join(ws.root, "Cargo.toml"), "rb") as f:
        wanted = tomllib.load(f).get("workspace", {}).get("lints", {})
    bad = []
    for pkg in sorted(ws.members, key=lambda p: p["name"]):
        with open(pkg["manifest_path"], "rb") as f:
            lints = tomllib.load(f).get("lints")
        if lints is None:
            bad.append(f"{pkg['name']} has no [lints]: `[lints] workspace = true`")
        elif lints.get("workspace") is not True and restated(lints) != wanted:
            bad.append(f"{pkg['name']}'s [lints] is not the workspace's table (its one allowed "
                       "difference is `rust.unsafe_code = \"deny\"`)")
    listed = git(ws.root, "ls-files", "-z", "--cached", "--others", "--exclude-standard")
    for rel in sorted(filter(None, listed.split("\0"))):
        name = os.path.basename(rel)
        if name == ".clippy.toml" or (name == "clippy.toml" and "/" in rel):
            bad.append(f"{rel}: clippy reads the nearest clippy.toml and does not merge; "
                       "the root clippy.toml is the only one")
        elif re.fullmatch(r"(.*/)?\.cargo/config(\.toml)?", rel):
            bad += [f"{rel}: {why}" for why in cargo_config(os.path.join(ws.root, rel))]
    return bad


def cargo_config(path):
    """What a cargo configuration does to the lint levels or the clippy
    configuration a build sees."""
    with open(path, encoding="utf-8", errors="replace") as f:
        text = f.read()
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        return [f"unreadable: {e}"]
    bad = []
    if "cap-lints" in text:
        bad.append("caps the lint levels (`cap-lints`)")
    if "CLIPPY_CONF_DIR" in (data.get("env") or {}):
        bad.append("sets CLIPPY_CONF_DIR; the root clippy.toml is the only one")

    def flags(node):
        if isinstance(node, dict):
            for key, value in node.items():
                if key in ("rustflags", "rustdocflags"):
                    yield from (value.split() if isinstance(value, str) else value)
                else:
                    yield from flags(value)

    for flag in flags(data):
        if flag in ("-A", "--allow", "--cap-lints") or flag.startswith(("-A", "--allow=")):
            bad.append(f"rustflags `{flag}` lowers a lint level")
    return bad


def against_base(config, base):
    """What `config` loosens against its `base` version."""
    if base is None:
        return []
    bad = [f"{config.path} adds known breach `{what}: {entry}`, which {base.path} does not "
           "hold: a breach is fixed, never listed"
           for what, entry in sorted(config.known() - base.known())]
    bad += [f"{config.path} drops binary {b}, which {base.path} lists"
            for b in base.binaries if b not in config.binaries]
    bad += [f"{config.path} drops test feature {f}, which {base.path} lists"
            for f in sorted(base.test_features - config.test_features)]
    return bad


def report(violations):
    for v in violations:
        print(f"purity: {v}", file=sys.stderr)
    return 1 if violations else 0


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("-C", dest="root", default=".", help="any directory of the repository")
    p.add_argument("--config", default="purity.toml", help="relative to the root")
    p.add_argument("--base", help="the revision the configuration is held to")
    p.add_argument("--staged", action="store_true")
    a = p.parse_args()
    root = git(a.root, "rev-parse", "--show-toplevel").strip()
    if a.staged:
        if a.base:
            fail_setup("--staged holds the configuration to HEAD; --base does not apply")
        blob = subprocess.run(["git", "-C", root, "show", f":{a.config}"], capture_output=True)
        config = Config(blob.stdout.decode(), a.config) if blob.returncode == 0 else \
            load_config(root, a.config)
        return report(against_base(config, base_config(root, default_base(root), a.config)))
    if a.base and not rev_exists(root, a.base):
        fail_setup(f"--base {a.base} is no commit")
    base = a.base or default_base(root)
    config = load_config(root, a.config)
    ws = Workspace(root)
    rc = report(ws.bad_tiers())
    rc = report(closure(ws, config)) or rc
    rc = report(pure_violations(ws, config)) or rc
    rc = report(lint_tables(ws)) or rc
    return report(against_base(config, base_config(root, base, a.config))) or rc


if __name__ == "__main__":
    sys.exit(main())
