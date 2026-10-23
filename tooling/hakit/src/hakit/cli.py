"""The `hakit` command line: validate, run, fire, verdict, cleanup (ADR-0049 §6.2), pcap, mark, the readers,
and inject and sipp-keys, the injection file of a catalog shape and the key defaults of a
scenario for a SIPp launcher outside the runner."""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
import time
from collections.abc import Mapping, Sequence
from datetime import UTC, datetime
from pathlib import Path

from . import content_cli
from .check import all_profile_errors
from .cleanup import cleanup
from .clock import RealClock
from .content import Content, ContentError, discover
from .dryrun import simulated, timeline
from .executor import RealExecutor
from . import jsonval as jv
from .jsonval import as_obj
from .injection import InjectionError, render
from .lab import lab_from_env, parse_callees
from .marks import mark
from .offschedule import active_runs, defaults_spec, request_fire
from .paths import results_root, search_path
from .profile import parse_profile
from .readers import cli as readers
from .runner import Runner, RunRefused
from .scenario_lint import key_defaults, lint_all
from .schemas import SchemaSet
from .jsonl import read_jsonl
from .pcap import extract as extract_call

# How long `hakit fire` waits for the runner, which takes requests between two
# waits: after an executable step, at most an inject timeout.
FIRE_WAIT_S = 300


def load(env: Mapping[str, str]) -> tuple[SchemaSet, Content]:
    """Schemas and the validated search path: the first step of every subcommand."""
    schemas = SchemaSet.load()
    errs = schemas.check_metaschema()
    if errs:
        raise ContentError(errs)
    content = discover(search_path(env), schemas)
    errs = lint_all(content.scenarios) + all_profile_errors(content)
    if errs:
        raise ContentError(errs)
    return schemas, content


def cmd_validate(content: Content) -> int:
    print(
        f"hakit: search path valid: {len(content.incidents)} incidents, {len(content.budgets)} budgets, "
        f"{len(content.profiles)} profiles, {len(content.catalogs)} catalogs, {len(content.scenarios)} scenario files, "
        f"{len(content.dashboards)} dashboards, {len(content.metric_families)} metric families"
    )
    return 0


def cmd_inject(content: Content, env: Mapping[str, str], shape: str, callees: Sequence[str]) -> int:
    """Print the SIPp injection file of a catalog shape, one row per callee."""
    s = content.shapes.get(shape)
    if s is None:
        print(f"hakit: no catalog on the search path has shape {shape!r}", file=sys.stderr)
        return 2
    hosts = parse_callees(",".join(callees)) if callees else lab_from_env(env).callees
    try:
        sys.stdout.write(render(s, content.catalogs[s.catalog], hosts))
    except InjectionError as e:
        print(f"hakit: {e}", file=sys.stderr)
        return 2
    return 0


def cmd_sipp_keys(content: Content, scenario: str) -> int:
    """Print `-key <name> <default>` for each key a scenario declares, one
    argument per line, for a launcher outside the runner."""
    path = content.scenarios.get(scenario)
    if path is None:
        print(f"hakit: no scenario {scenario!r} on the search path", file=sys.stderr)
        return 2
    for k, v in sorted(key_defaults(path).items()):
        print(f"-key\n{k}\n{v}")
    return 0


def cmd_run(schemas: SchemaSet, content: Content, env: Mapping[str, str], profile: str, dry_run: bool) -> int:
    lab = lab_from_env(env)
    if dry_run:
        clock, fake = simulated(shapes=sorted(content.shapes))
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
    clock, executor = simulated() if dry_run else (RealClock(), RealExecutor())
    return cleanup(content, lab_from_env(env), executor, clock, results_root(env), run_dirs, dry_run)


def cmd_fire(content: Content, env: Mapping[str, str], incident: str, run_dir: Path | None, dry_run: bool) -> int:
    dirs = [run_dir] if run_dir is not None else active_runs(results_root(env))
    if len(dirs) != 1:
        print(f"hakit: fire refused: {len(dirs)} running runs; name one with --run", file=sys.stderr)
        return 2
    target = dirs[0]
    profile = parse_profile(as_obj(json.loads((target / "profile.json").read_text())))
    if dry_run:
        ok = incident in content.incidents and defaults_spec(profile, incident) is not None
        print(f"hakit: dry run: {incident} would be {'requested' if ok else 'refused'} in {target.name}")
        return 0 if ok else 2
    nonce = datetime.now(UTC).strftime("%Y%m%dT%H%M%S%fZ")
    answer = request_fire(target, incident, RealClock(), FIRE_WAIT_S, nonce)
    if answer.get("accepted") is True:
        print(f"hakit: {target.name}: event {answer.get('event')} fired")
        return 0
    print(f"hakit: fire refused: {answer.get('reason')}", file=sys.stderr)
    return 2


def cmd_pcap(run_dir: Path, call_id: str, out: Path | None) -> int:
    written = extract_call(run_dir, call_id, out or run_dir / "pcap" / "calls")
    for w in written:
        print(f"hakit: {w}")
    if not written:
        print(f"hakit: no packet of {call_id} under {run_dir / 'pcap'}", file=sys.stderr)
    return 0 if written else 1


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="hakit", description="Pluggable HA tooling base (ADR-0049).")
    ap.add_argument("--dry-run", action="store_true", help="fake clock and simulated lab; no effect leaves the runner")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("validate", help="validate every file of the search path")
    p_run = sub.add_parser("run", help="run a profile")
    p_run.add_argument("profile")
    p_ver = sub.add_parser("verdict", help="recompute verdict.json from a run directory")
    p_ver.add_argument("run_dir", type=Path)
    p_fire = sub.add_parser("fire", help="fire one manual event in the running run")
    p_fire.add_argument("incident")
    p_fire.add_argument("--run", type=Path, help="the run directory, when several runs are running")
    p_pcap = sub.add_parser("pcap", help="one call's packets out of a run's capture, by Call-ID")
    p_pcap.add_argument("run_dir", type=Path)
    p_pcap.add_argument("call_id")
    p_pcap.add_argument("-o", "--out", type=Path, help="the output directory (default: <run_dir>/pcap/calls)")
    p_inj = sub.add_parser("inject", help="print the SIPp injection file of a catalog shape")
    p_inj.add_argument("shape")
    p_inj.add_argument("--callee", action="append", default=[], help="host[:port] of a callee (default: the lab's)")
    p_keys = sub.add_parser("sipp-keys", help="print the -key defaults a scenario declares, one argument per line")
    p_keys.add_argument("scenario")
    p_clean = sub.add_parser("cleanup", help="heal every event a crashed run left open")
    p_clean.add_argument("run_dirs", type=Path, nargs="*")
    p_mark = sub.add_parser("mark", help="push the state of a workflow supervising a run (phase, progress, finding, annotate)")
    p_mark.add_argument("kind", choices=["phase", "progress", "finding", "annotate"])
    p_mark.add_argument("values", nargs="+")
    content_cli.add_parsers(sub)
    readers.add_parsers(sub)
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
    if cmd in content_cli.COMMANDS:
        return content_cli.run(cmd, args, content)
    if cmd == "run":
        return cmd_run(schemas, content, env, str(args.profile), dry)
    if cmd == "inject":
        return cmd_inject(content, env, str(args.shape), [str(c) for c in args.callee])
    if cmd == "sipp-keys":
        return cmd_sipp_keys(content, str(args.scenario))
    if cmd == "verdict":
        return cmd_verdict(schemas, content, env, Path(args.run_dir))
    if cmd == "pcap":
        return cmd_pcap(Path(args.run_dir), str(args.call_id), None if args.out is None else Path(args.out))
    if cmd in readers.COMMANDS:
        return readers.run(cmd, args, env, RealExecutor())
    if cmd == "mark":
        lab = lab_from_env(env)
        grafana = env.get("HAKIT_GRAFANA_URL", "http://127.0.0.1:3333")
        values = [str(v) for v in args.values]
        return mark(RealExecutor(), lab.vm_url, grafana, str(args.kind), values, int(time.time() * 1000))
    if cmd == "fire":
        run = None if args.run is None else Path(args.run)
        return cmd_fire(content, env, str(args.incident), run, dry)
    return cmd_cleanup(content, env, [Path(p) for p in args.run_dirs], dry)


if __name__ == "__main__":
    sys.exit(main())
