"""What the base counted, so a site a tightened rule reveals is told from one
the branch wrote (ADR-0051 §3).

A site is revealed when the base did not count its lint (read from the
base's `scripts/panic_sites_lints.py`), or when its message names a list
entry of the root clippy.toml that the base did not hold, or held for the
pure tier only while the site's crate is a shell crate. A base whose lints
module or clippy.toml cannot be read reveals nothing.
"""
import ast
import os
import re
import subprocess

from panic_sites_lists import PURE_ONLY, entries as list_entries

LINTS_PATH = "scripts/panic_sites_lints.py"
LIST_KEYS = {"clippy::disallowed_macros": "disallowed-macros",
             "clippy::disallowed_methods": "disallowed-methods",
             "clippy::disallowed_types": "disallowed-types"}
NAMED = re.compile(r"`([^`]+)`")


def show(repo, spec):
    """The text of `spec` (`rev:path`) in `repo`, or None."""
    out = subprocess.run(["git", "-C", repo, "show", spec], capture_output=True)
    return out.stdout.decode() if out.returncode == 0 else None


def lints_text(root, base):
    """The base's lints module: the root's own at the base, else the one of
    the repository holding this script at the commit the base pins it at."""
    text = show(root, f"{base}:{LINTS_PATH}")
    if text is not None:
        return text
    here = os.path.dirname(os.path.abspath(__file__))
    top = subprocess.run(["git", "-C", here, "rev-parse", "--show-toplevel"],
                         capture_output=True, text=True)
    if top.returncode != 0:
        return None
    repo = top.stdout.strip()
    rel = os.path.relpath(repo, root)
    if rel.startswith(os.pardir) or rel == os.curdir:
        return None
    pinned = subprocess.run(["git", "-C", root, "rev-parse", f"{base}:{rel}"],
                            capture_output=True, text=True)
    if pinned.returncode != 0:
        return None
    return show(repo, f"{pinned.stdout.strip()}:{LINTS_PATH}")


def counted_lints(text):
    """`(both tiers, pure tier only)`: the lint codes a lints module counts."""
    values = {}
    for node in ast.parse(text).body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and \
                isinstance(node.targets[0], ast.Name):
            try:
                values[node.targets[0].id] = ast.literal_eval(node.value)
            except ValueError:
                continue
    both = tuple(values.get("DENIED", ())) + tuple(values.get("LISTS", ()))
    return ({f"clippy::{lint}" for lint in both},
            {f"clippy::{lint}" for lint in values.get("PURE_DENIED", ())})


class Base:
    """The rules the base counted: its lints and its list entries, each
    None when unreadable."""

    def __init__(self, lints, lists, now_lists):
        self.lints, self.lists, self.now_lists = lints, lists, now_lists

    @classmethod
    def at(cls, root, base, now_text):
        lints = lists = now = None
        if base is not None:
            text = lints_text(root, base)
            try:
                lints = None if text is None else counted_lints(text)
            except SyntaxError:
                lints = None
            then = show(root, f"{base}:clippy.toml")
            try:
                lists = None if then is None else list_entries(then, "base clippy.toml")
                now = None if now_text is None else list_entries(now_text, "clippy.toml")
            except ValueError:
                lists = now = None
        return cls(lints, lists, now)

    def counted(self, code, message, tier):
        """Whether the base counted a site of `code` saying `message` in a
        crate of `tier`."""
        if self.lints is not None:
            both, pure = self.lints
            if code not in both and not (tier == "pure" and code in pure):
                return False
        key = LIST_KEYS.get(code)
        named = NAMED.search(message) if key else None
        if named is None or self.lists is None or self.now_lists is None:
            return True
        entry = (key, named.group(1))
        if entry not in self.now_lists:
            return True
        reason = self.lists.get(entry)
        return reason is not None and (tier == "pure" or not reason.startswith(PURE_ONLY))

    def tightened(self, now_lints):
        """Whether the current rules count anything the base did not."""
        if self.lints is not None and now_lints is not None:
            for now, then in zip(now_lints, self.lints):
                if now - then:
                    return True
        if self.lists is None or self.now_lists is None:
            return False
        return any(entry not in self.lists or (
            self.lists[entry].startswith(PURE_ONLY) and not reason.startswith(PURE_ONLY))
            for entry, reason in self.now_lists.items())


def revealed(found, base):
    """`{file: sites}` of the sites the base did not count."""
    sites = {}
    for f in found:
        if not base.counted(f.code, f.message, f.crate.tier):
            sites[f.file] = sites.get(f.file, 0) + 1
    return sites
