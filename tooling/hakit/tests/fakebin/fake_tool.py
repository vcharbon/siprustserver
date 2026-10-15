#!/usr/bin/env python3
"""A stand-in for `kubectl` and `docker` in the incident executable tests.

Invoked through a symlink named after the tool. Appends its argv (the tool
name first) and what it read on stdin to `$FAKE_LOG`, one JSON object a line,
and answers from the rules in `$FAKE_RULES` (a JSON list of
`{"match", "stdout", "rc", "once"}`): the first rule whose regular expression
matches the space-joined argv decides; a `once` rule is spent after its first
match. Without a match: no output, exit 0.
"""

from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path
from typing import cast


def main() -> int:
    argv = [Path(sys.argv[0]).name, *sys.argv[1:]]
    data = sys.stdin.read() if "-f" in argv and "-" in argv else ""
    with open(os.environ["FAKE_LOG"], "a") as log:
        log.write(json.dumps({"argv": argv, "stdin": data}) + "\n")
    rules_path = Path(os.environ["FAKE_RULES"])
    rules = cast(list[dict[str, object]], json.loads(rules_path.read_text()))
    joined = " ".join(argv)
    for i, rule in enumerate(rules):
        if re.search(str(rule["match"]), joined):
            if rule.get("once"):
                rules.pop(i)
                rules_path.write_text(json.dumps(rules))
            sys.stdout.write(str(rule.get("stdout", "")))
            return int(str(rule.get("rc", 0)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
