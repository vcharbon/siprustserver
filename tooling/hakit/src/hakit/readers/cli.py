"""The readers' `hakit` subcommands: drift, chart, logs, obs-check."""

from __future__ import annotations

import argparse
import sys
from collections.abc import Mapping
from dataclasses import replace
from pathlib import Path

from .. import jsonval as jv
from ..executor import Executor
from .chart import ChartRequest, chart
from .drift import Drift
from .drift_spec import load_spec
from .logs import format_line, incarnations, query
from .obs_check import check, load_scopes, selectors
from .run_view import read_run
from .vmrange import Vm

COMMANDS = ("drift", "chart", "logs", "obs-check")
DRIFT_EXIT = {"pass": 0, "warn": 0, "fail": 1, "na": 3}


def add_parsers(sub: argparse._SubParsersAction[argparse.ArgumentParser]) -> None:  # pyright: ignore[reportPrivateUsage]
    p = sub.add_parser("drift", help="drift and leak verdicts of a run, from VictoriaMetrics and its records")
    p.add_argument("run_dir", type=Path)
    p.add_argument("--spec", type=Path, help="YAML overriding the base's pods, containers and bounds")
    p.add_argument("--namespace")
    p.add_argument("--warmup", type=int, help="seconds after the load start left out")
    p = sub.add_parser("chart", help="a VictoriaMetrics range query as an inline SVG on stdout")
    p.add_argument("--title", required=True)
    p.add_argument("--expr", action="append", default=[])
    p.add_argument("--legend", action="append", default=[])
    p.add_argument("--run", type=Path, help="a run directory: its span, windows and phases")
    p.add_argument("--start", type=float)
    p.add_argument("--end", type=float)
    p.add_argument("--step", type=float, default=30)
    p.add_argument("--unit", default="")
    p.add_argument("--marker", action="append", default=[], help="<epoch>:<label>")
    p = sub.add_parser("logs", help="a window of pod logs from VictoriaLogs, every incarnation")
    p.add_argument("t0", type=int)
    p.add_argument("t1", type=int)
    p.add_argument("pod_regex")
    p.add_argument("out_log", type=Path)
    p.add_argument("--namespace", default="sip-test")
    p = sub.add_parser("obs-check", help="is every series of the scopes in VictoriaMetrics, fresh?")
    p.add_argument("scopes", nargs="+")
    p.add_argument("--spec", type=Path, help="YAML adding the product's scopes")
    p.add_argument("--namespace", default="sip-test")
    p.add_argument("--wait", type=float, default=180)
    p.add_argument("--list", action="store_true", help="print the selectors only")


def run(cmd: str, args: argparse.Namespace, env: Mapping[str, str], executor: Executor) -> int:
    vm_url = env.get("HAKIT_VM_URL", "http://127.0.0.1:8428")
    if cmd == "drift":
        spec = load_spec(args.spec)
        if args.namespace or args.warmup is not None:
            spec = replace(
                spec,
                namespace=args.namespace or spec.namespace,
                warmup_s=args.warmup if args.warmup is not None else spec.warmup_s,
            )
        run_dir = Path(args.run_dir)
        doc = Drift(read_run(run_dir), Vm(executor, vm_url), spec, run_dir / "drift").run()
        for c in jv.objs(doc, "checks"):
            print(f"{c['verdict']:5} {c['name']}: {c['detail']}")
        print(f"drift verdict: {doc['verdict']} ({run_dir / 'drift' / 'drift.json'})")
        return DRIFT_EXIT[str(doc["verdict"])]
    if cmd == "chart":
        view = read_run(Path(args.run)) if args.run else None
        req = ChartRequest(args.title, list(args.expr), list(args.legend), args.start, args.end, args.step, args.unit, tuple(args.marker))
        try:
            print(chart(Vm(executor, vm_url), req, view))
        except ValueError as e:
            print(f"hakit chart: {e}", file=sys.stderr)
            return 2
        return 0
    if cmd == "logs":
        vl = env.get("HAKIT_VLOGS_URL", "http://127.0.0.1:9428")
        out = Path(args.out_log)
        try:
            recs = query(executor, vl, str(args.namespace), int(args.t0), int(args.t1), str(args.pod_regex))
        except ValueError as e:
            out.write_text(f"VictoriaLogs unread at {vl}: {e}\n")
            return 1
        out.write_text("".join(format_line(r) + "\n" for r in recs))
        print("\n".join(incarnations(recs)))
        return 0
    scopes = load_scopes(args.spec)
    try:
        series = selectors(scopes, list(args.scopes), str(args.namespace))
    except ValueError as e:
        print(f"hakit obs-check: {e}", file=sys.stderr)
        return 2
    if args.list:
        print("\n".join(series))
        return 0
    res = check(executor, vm_url, series, float(args.wait))
    sys.stderr.write(res.stderr)
    return res.rc
