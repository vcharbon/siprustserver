#!/usr/bin/env python3
"""The failover sweep's report (ADR-0055 §11): every sweep test's cells added
together, per row of the window table and per tier. Gates nothing.

  sweep-report.py [DIR]

DIR is the directory the sweep tests write to (default `target/sweep-report`):
one JSON file per test, `{"test": prefix, "cells": [{"cell", "tier", "fired",
"evidence", "window_ms", "narrow", "passed", "failure"}]}`. Prints, per level
and tier, the cells, how many failed and how many are cut in a narrow window
(between two sends of one turn's effects, under 1 ms apart); per row, the cells it fired in
by level and tier, the narrow ones apart; then every failing Guaranteed cell
with the rows it fired, and every failing narrow cell.
"""

import json
import sys
from collections import Counter, defaultdict
from pathlib import Path


def cells(root):
    for path in sorted(root.glob("*.json")):
        doc = json.loads(path.read_text())
        for cell in doc["cells"]:
            # <shape>/<level>/<incident>/t<k>-<point>
            cell["level"] = cell["cell"].split("/")[1]
            yield cell


def main():
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "target/sweep-report")
    if not root.is_dir():
        sys.exit(f"sweep-report: no {root}: run the sweep tests first")
    tiers = Counter()
    failed = Counter()
    narrow = Counter()
    rows = defaultdict(Counter)
    narrow_rows = defaultdict(Counter)
    guaranteed_failures = []
    narrow_failures = []
    for cell in cells(root):
        key = (cell["level"], cell["tier"] or "uncut")
        tiers[key] += 1
        is_narrow = cell.get("narrow", False)
        if is_narrow:
            narrow[key] += 1
        if not cell["passed"]:
            failed[key] += 1
            if cell["tier"] == "guaranteed":
                guaranteed_failures.append(cell)
            if is_narrow:
                narrow_failures.append(cell)
        for row in cell["fired"]:
            (narrow_rows if is_narrow else rows)[row][key] += 1

    print("cells per level and tier (failing, narrow):")
    for key in sorted(tiers):
        print(f"  {key[0]:8} {key[1]:11} {tiers[key]:6} ({failed[key]}, {narrow[key]})")
    print("\ncells each row fired in, per level and tier (narrow cells apart):")
    for row in sorted(set(rows) | set(narrow_rows)):
        parts = ", ".join(f"{lvl}/{tier} {n}" for (lvl, tier), n in sorted(rows[row].items()))
        apart = ", ".join(
            f"{lvl}/{tier} {n}" for (lvl, tier), n in sorted(narrow_rows[row].items())
        )
        print(f"  {row:30} {parts}" + (f"  | narrow: {apart}" if apart else ""))
    print(f"\nfailing guaranteed cells: {len(guaranteed_failures)}")
    for cell in guaranteed_failures:
        print(f"  {cell['cell']}: {cell.get('failure') or ''}")
    print(f"\nfailing narrow cells: {len(narrow_failures)}")
    for cell in narrow_failures:
        print(f"  {cell['cell']} ({cell.get('window_ms')} ms): {cell.get('failure') or ''}")


if __name__ == "__main__":
    main()
