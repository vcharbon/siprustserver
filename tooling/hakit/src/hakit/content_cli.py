"""The `hakit` subcommands that read the validated search path itself: `path` and `dashboards`."""

from __future__ import annotations

import argparse
from pathlib import Path

from .content import Content
from .grafana import compose_override

COMMANDS = ("path", "dashboards")


def add_parsers(sub: argparse._SubParsersAction[argparse.ArgumentParser]) -> None:  # pyright: ignore[reportPrivateUsage]
    sub.add_parser("path", help="print the search path, one entry per line")
    p = sub.add_parser("dashboards", help="list the dashboards of the search path (uid and file)")
    p.add_argument("--compose-override", type=Path, help="write the Grafana compose override mounting them in place")


def run(cmd: str, args: argparse.Namespace, content: Content) -> int:
    if cmd == "path":
        for entry in content.path:
            print(entry)
        return 0
    for uid, p in sorted(content.dashboards.items()):
        print(f"{uid}\t{p}")
    out = args.compose_override
    if out is not None:
        target = Path(out)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(compose_override(content.dashboard_dirs))
    return 0
