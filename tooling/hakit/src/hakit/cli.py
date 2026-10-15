"""The `hakit` command line: validate, run, verdict, cleanup (ADR-0049 §6.2)."""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from collections.abc import Mapping, Sequence
from pathlib import Path

from .check import all_profile_errors
from .clock import RealClock
from .content import Content, ContentError, discover
from .dryrun import simulated, timeline
from .executor import RealExecutor
from .incident_exec import EventRunner
from . import jsonval as jv
from .jsonval import as_obj
from .lab import lab_from_env
from .paths import results_root, search_path
from .preflight import open_events
from .profile import parse_profile
from .runner import Runner, RunRefused
from .schedule import plan
from .schemas import SchemaSet
from .verdict import read_jsonl
from .vm import VictoriaMetrics
from .events import EventJournal


def load(env: Mapping[str, str]) -> tuple[SchemaSet, Content]:
    """Schemas and the validated search path: the first step of every subcommand."""
    schemas = SchemaSet.load()
    errs = schemas.check_metaschema()
    if errs:
        raise ContentError(errs)
    content = discover(search_path(env), schemas)
    errs = all_profile_errors(content)
    if errs:
        raise ContentError(errs)
    return schemas, content


def cmd_validate(content: Content) -> int:
    print(
        f"hakit: search path valid: {len(content.incidents)} incidents, {len(content.budgets)} budgets, "
        f"{len(content.profiles)} profiles, {len(content.catalogs)} catalogs, {len(content.scenarios)} scenario files"
    )
    return 0


def cmd_run(schemas: SchemaSet, content: Content, env: Mapping[str, str], profile: str, dry_run: bool) -> int:
    lab = lab_from_env(env)
    if dry_run:
        clock, fake = simulated()
        results = Path(tempfile.mkdtemp(prefix="hakit-dry-run-"))
        runner = Runner(content, schemas, lab, fake, clock, results)
    else:
        fake = None
        runner = Runner(content, schemas, lab, RealExecutor(), RealClock(), results_root(env))
    try:
        run_dir = runner.run(profile)
    except RunRefused as e:
        print(f"hakit: run refused: {e}", file=sys.stderr)
        return 2
    verdict = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    if fake is not None:
        started = int(jv.number(verdict, "started_at_ms"))
        print("\n".join(timeline(read_jsonl(run_dir / "events.jsonl"), started)))
        for w in jv.objs(verdict, "windows"):
            print(f"window {w['event']}: {w['opened_at_ms']} .. {w['closed_at_ms']} ({w['status']})")
        print(f"hakit: dry run: {len(fake.calls)} effects recorded, run directory {run_dir}")
    print(f"hakit: verdict {verdict['status']}: {run_dir / 'verdict.json'}")
    return 0 if verdict["status"] == "pass" else 1


def cmd_verdict(schemas: SchemaSet, content: Content, env: Mapping[str, str], run_dir: Path) -> int:
    runner = Runner(content, schemas, lab_from_env(env), RealExecutor(), RealClock(), run_dir.parent)
    v = runner.write_verdict(run_dir)
    print(f"hakit: verdict {v['status']}: {run_dir / 'verdict.json'}")
    return 0 if v["status"] == "pass" else 1


def cmd_cleanup(content: Content, env: Mapping[str, str], run_dirs: Sequence[Path], dry_run: bool) -> int:
    lab = lab_from_env(env)
    clock, executor = simulated() if dry_run else (RealClock(), RealExecutor())
    root = results_root(env)
    dirs = list(run_dirs) or ([d for d in sorted(root.iterdir()) if d.is_dir()] if root.is_dir() else [])
    failed = 0
    for d in dirs:
        ids = open_events(d)
        if not ids:
            continue
        run = d.name
        profile = parse_profile(as_obj(json.loads((d / "profile.json").read_text())))
        journal = EventJournal(run, d, clock, VictoriaMetrics(executor, lab.vm_url))
        stepper = EventRunner(run, d, lab, executor, clock, journal, lambda _t: None)
        for ev in plan(profile, content.incidents):
            if ev.event_id in ids:
                bad = stepper.heal_only(ev, [content.incidents[n] for n in ev.spec.incidents])
                print(f"hakit: {run}/{ev.event_id} healed" + (f", failed: {bad}" if bad else ""))
                failed += len(bad)
        ps = executor.run(["docker", "ps", "-aq", "--filter", f"label=hakit-run={run}"], timeout_s=30)
        ids_c = ps.stdout.split()
        if ids_c:
            executor.run(["docker", "rm", "-f", *ids_c], timeout_s=60)
        if not failed and not dry_run:
            (d / "cleaned").write_text("")
    return 1 if failed else 0


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="hakit", description="Pluggable HA tooling base (ADR-0049).")
    ap.add_argument("--dry-run", action="store_true", help="fake clock and simulated lab; no effect leaves the runner")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("validate", help="validate every file of the search path")
    p_run = sub.add_parser("run", help="run a profile")
    p_run.add_argument("profile")
    p_ver = sub.add_parser("verdict", help="recompute verdict.json from a run directory")
    p_ver.add_argument("run_dir", type=Path)
    p_clean = sub.add_parser("cleanup", help="heal every event a crashed run left open")
    p_clean.add_argument("run_dirs", type=Path, nargs="*")
    args = ap.parse_args(argv)
    env = dict(os.environ)
    try:
        schemas, content = load(env)
    except ContentError as e:
        print("hakit: refused:\n" + "\n".join(f"  {x}" for x in e.errors), file=sys.stderr)
        return 2
    cmd = str(args.cmd)
    dry = bool(args.dry_run)
    if cmd == "validate":
        return cmd_validate(content)
    if cmd == "run":
        return cmd_run(schemas, content, env, str(args.profile), dry)
    if cmd == "verdict":
        return cmd_verdict(schemas, content, env, Path(args.run_dir))
    return cmd_cleanup(content, env, [Path(p) for p in args.run_dirs], dry)


if __name__ == "__main__":
    sys.exit(main())
