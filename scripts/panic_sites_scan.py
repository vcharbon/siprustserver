"""The loop and recursion sites of the tiered crates (ADR-0057 §5), counted
by the syntax scanner `tooling/site-scan` beside the clippy pass.

A crate's loop sites count when it is pure, or a service built on the SDK (a
crate that depends on `b2bua-sdk` and not on `b2bua-core`, as the B2BUA's own
crates do), `bounds` aside: its `bounded` is the loop the set relies on.
Recursion sites count in every tiered crate. The scanner prints one JSON
line per site, `reason` `site-scan`, with the span fields of
a clippy span; the pass records them after clippy's lines, and `--json`
reads both back.
"""
import json
import os
import subprocess

from panic_sites_lints import SITE_SETS

SDK, CORE, BOUNDS = "b2bua-sdk", "b2bua-core", "bounds"
REASON = "site-scan"
MANIFEST = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tooling", "site-scan",
                        "Cargo.toml")
LOOP, RECURSION = (f"site::{s}" for s in SITE_SETS)


def sets_of(crate):
    """The site sets `crate` is counted in."""
    service = SDK in crate.deps and CORE not in crate.deps
    loops = (crate.tier == "pure" or service) and crate.name != BOUNDS
    return [s for s in SITE_SETS if s == "recursion" or loops]


def run(root, crates):
    """`(exit code, the scanner's lines, its stderr)` over `crates` under
    `root`."""
    request = {"root": root, "crates": [
        {"name": c.name, "roots": c.roots, "sets": sets_of(c)} for c in crates]}
    out = subprocess.run(["cargo", "run", "--quiet", "--manifest-path", MANIFEST],
                         input=json.dumps(request), cwd=root, capture_output=True, text=True)
    return out.returncode, out.stdout, out.stderr


def sites_of(text, crates):
    """`(crate, site record)` of each scanner line of `text` naming one of
    `crates`, in the sets that crate is counted in."""
    by_name = {c.name: c for c in crates}
    out = []
    for raw in text.splitlines():
        if not raw.startswith("{"):
            continue
        rec = json.loads(raw)
        crate = by_name.get(rec.get("crate"))
        if rec.get("reason") != REASON or crate is None or rec.get("set") not in sets_of(crate):
            continue
        out.append((crate, rec))
    return out


def rendered(rec):
    """The site as the pass prints it."""
    return (f"{rec['set']} site: {rec['message']}\n"
            f" --> {rec['file_name']}:{rec['line_start']}:{rec['column_start']}\n")


HINT = ("a loop is a `for` over a finite collection or a range to a constant or a `len()`, or "
        "`bounds::bounded`; a recursion takes a `bounds::Depth` and descends through it, with a "
        "test at its maximum depth (ADR-0057 §5)")
