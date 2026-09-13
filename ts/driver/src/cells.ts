/**
 * Running ONE cell of the matrix, in either of the two shapes a campaign holds.
 *
 * - a **pivot-replay** cell compiles a run-spec, hands it to the interpreter and
 *   reads the verdict the bundle carries;
 * - a **rust-test** cell runs one crate test and reads the `result.json` the
 *   crate's own in-test reporter wrote, falling back to the exit code where no
 *   reporter is wired.
 *
 * Both land on the same {@link E2e.CellSummary}, because a campaign is ONE
 * matrix: the two shapes share a run dir and fold into one index.
 *
 * A cell that never produced a result is not silently red. It leaves an
 * `error.txt` beside where its bundle would have been, and the summary carries
 * the same line, so a reader never has to guess whether a cell failed or never
 * ran. A cell whose document declares its lane BLOCKED is not red either: it
 * leaves a `skipped.json` and never reaches the interpreter.
 */
import { Body, Bundle, Campaign, CellHits, Confrontation, E2e, Flows, Pivot, Tokens } from "@sip/contracts"
import { Classifier, Confront, Reclassifier } from "@sip/pipeline"
import { ReplayCli, runner as toolchainRunner } from "@sip/toolchain"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import { LanePresets } from "./lanes.js"
import {
  cellDir,
  cellIdOf,
  CLASSIFICATION_FILE,
  CONFRONTATION_FILE,
  ERROR_FILE,
  RECORDING_DIR,
  RESULT_FILE,
  RFC_FILE,
  RULE_HITS_FILE,
  runSpecFile,
  SKIP_FILE,
  SPECS_DIR,
  VERDICT_FILE
} from "./layout.js"
import { RoutingCompiler } from "./routing.js"
import { emitRunSpec } from "./run-spec.js"

/**
 * How a campaign invokes a crate's tests. Data rather than a service: WHERE the
 * workspace is and WHICH runner it uses is a fact about a checkout, and a
 * campaign that has to state it should state it in one visible place.
 */
export interface TestInvocation {
  readonly command: string
  /** The whole argv after `command`, built from the cell. */
  readonly args: (cell: Campaign.RustTestCell) => ReadonlyArray<string>
  readonly cwd?: string
  /**
   * The env var the crate's in-test reporter reads to find its own cell
   * directory. Unset means no reporter is wired, and the exit code is the
   * verdict.
   */
  readonly cellDirEnv?: string
  /**
   * The env var the reporter reads for the infra half of its cell id. The
   * driver owns the label — the reporter echoes what it was given — so the
   * index and the result record can never disagree on it.
   */
  readonly cellInfraEnv?: string
}

/** `cargo test -p <crate> -- --exact <name> --nocapture`, the standard form. */
export const cargoTest: TestInvocation = {
  command: "cargo",
  args: (cell) => ["test", "-p", cell.crate, "--", "--exact", cell.name, "--nocapture"]
}

/** Everything one cell needs that the campaign decided once. */
export interface CellContext {
  /** Absolute path of the campaign's run directory. */
  readonly runDir: string
  readonly tests: TestInvocation
  /**
   * Whether classification decides a replay cell's outcome. In `record` mode
   * every confrontation lands on the record and nothing fails because of one;
   * in `enforce` mode an `unlisted` or `unknown` record fails the cell.
   */
  readonly mode: Confrontation.ConfrontMode
}

/** One finished cell: the index line, and what a reader is told about it. */
export interface CellRun {
  readonly summary: E2e.CellSummary
  /** Non-empty exactly where something is worth reading — a refusal, a stack. */
  readonly detail: string
}

export const runCell = Effect.fn("Driver.runCell")(function* (
  cell: Campaign.CampaignCell,
  context: CellContext
) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const dir = cellDir(cell)
  const absolute = path.join(context.runDir, dir)

  const body: Effect.Effect<CellOutcome, unknown, CellServices> =
    cell.kind === "rust-test"
      ? runRustTest(cell, context, absolute)
      : runPivotReplay(cell, context, absolute)
  const outcome = yield* guarded(body)

  if (outcome.crashed) {
    yield* fs.makeDirectory(absolute, { recursive: true })
    yield* fs.writeFileString(path.join(absolute, ERROR_FILE), `${outcome.detail}\n`)
  }
  if (outcome.skipped !== undefined) {
    yield* fs.makeDirectory(absolute, { recursive: true })
    yield* fs.writeFileString(
      path.join(absolute, SKIP_FILE),
      E2e.emitCellSkip({ cell: cellIdOf(cell), verdict: outcome.skipped })
    )
  }

  return {
    summary: {
      cell: cellIdOf(cell),
      passed: outcome.passed,
      dir,
      ...(outcome.crashed ? { error: firstLine(outcome.detail) } : {}),
      ...(outcome.skipped === undefined ? {} : { skipped: outcome.skipped })
    },
    detail: outcome.detail
  } satisfies CellRun
})

/** Everything either cell shape may reach for. */
type CellServices =
  | FileSystem.FileSystem
  | Path.Path
  | ChildProcessSpawner.ChildProcessSpawner
  | ReplayCli.Service
  | LanePresets.Service
  | RoutingCompiler.Service
  | Reclassifier.Service
  | Classifier.Service

interface CellOutcome {
  readonly passed: boolean
  readonly detail: string
  /** Whether the cell never reached a result at all. */
  readonly crashed: boolean
  /**
   * The lane verdict that kept the cell from running; absent where it ran. A
   * skipped cell is the third outcome: `passed` is false because nothing passed,
   * and no campaign fails because of it.
   */
  readonly skipped?: Tokens.LaneVerdict
}

/**
 * A cell's own failure never fails the CAMPAIGN: it becomes a crashed cell, with
 * the whole cause written down. A campaign that died on its third cell would
 * leave the other cells' work unreadable.
 */
const guarded = <R>(
  body: Effect.Effect<CellOutcome, unknown, R>
): Effect.Effect<CellOutcome, never, R> =>
  body.pipe(
    Effect.catchCause((cause) =>
      Effect.succeed<CellOutcome>({
        passed: false,
        detail: `the cell produced no result:\n${Cause.pretty(cause)}`,
        crashed: true
      })
    )
  )

/**
 * One document on one lane. The cell directory IS the interpreter's `out_dir`,
 * so the run-spec is written under `specs/` and nothing else is put there before
 * the run.
 */
const runPivotReplay = Effect.fn("Driver.runPivotReplay")(function* (
  cell: Campaign.PivotReplayCell,
  context: CellContext,
  absolute: string
) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const replay = yield* ReplayCli.Service
  const lanes = yield* LanePresets.Service
  const routing = yield* RoutingCompiler.Service
  const reclassifier = yield* Reclassifier.Service

  const pivot = yield* Pivot.parsePivot(yield* fs.readFileString(yield* documentPath(cell.case)))

  const blocked = laneBlock(pivot, cell.lane)
  if (blocked !== undefined) {
    return {
      passed: false,
      detail: `skipped: the document declares lane ${cell.lane} ${blocked}`,
      crashed: false,
      skipped: blocked
    } satisfies CellOutcome
  }

  const lane = yield* lanes.laneOf(cell)
  const overlay = yield* routing.compile(cell, pivot)

  const specDir = path.join(context.runDir, SPECS_DIR)
  yield* fs.makeDirectory(specDir, { recursive: true })
  const specPath = path.join(specDir, runSpecFile(cell))
  yield* fs.writeFileString(
    specPath,
    emitRunSpec({ case: cell.case, out_dir: absolute, lane, run: overlay })
  )

  const report = yield* replay.run(specPath)
  if (!ReplayCli.leftABundle(report.outcome)) {
    return {
      passed: false,
      detail:
        `replay exited ${report.exitCode} (${report.outcome._tag}) and left no bundle:\n` +
        report.stderr,
      crashed: true
    } satisfies CellOutcome
  }

  const verdictPath = path.join(absolute, VERDICT_FILE)
  const verdict = yield* Bundle.decodeRunVerdict(
    JSON.parse(yield* fs.readFileString(verdictPath)) as unknown
  )
  // The lane's post-run RFC audit is the one instrument that sees a SUT
  // violation the scripted peers answer without complaint: a gating finding
  // fails the cell exactly as a failed verdict does. A bundle without the file
  // was not audited, and says nothing.
  const rfcPath = path.join(absolute, RFC_FILE)
  const rfcGating = (yield* fs.exists(rfcPath))
    ? Bundle.rfcGating(
      yield* Bundle.decodeRunRfcAudit(JSON.parse(yield* fs.readFileString(rfcPath)) as unknown)
    )
    : []
  const structural = Bundle.verdictPassed(verdict) && rfcGating.length === 0

  const caseId = Campaign.cellCaseId(cell)
  const classification = yield* confrontCell(cell, context, absolute, pivot, verdict, caseId)
  const gated = context.mode === "enforce" ? structural && classification.passed : structural

  const final = yield* reclassifier.reclassify({
    caseId,
    lane: cell.lane,
    verdict,
    dir: absolute,
    passed: gated
  })
  // The deployment's counts ride the same reading as its verdict, so whoever
  // aggregates a run later reads one small file per cell rather than the bundle.
  if (final.hits !== undefined) {
    yield* fs.writeFileString(path.join(absolute, RULE_HITS_FILE), CellHits.emitHits(final.hits))
  }
  return {
    passed: final.passed,
    detail:
      final.passed === gated
        ? gated === structural
          ? rfcGating.length === 0
            ? report.stderr
            : `the RFC audit failed the cell (see ${RFC_FILE}):\n` +
              rfcGating.map((f) => `  ${f.rule} [${f.lane}]: ${f.detail}`).join("\n")
          : `classification failed the cell: ${classification.unlisted} unlisted, ` +
            `${classification.unknown} unknown (see ${CONFRONTATION_FILE})`
        : `${verdict.status} restated as ${final.passed ? "passed" : "failed"}: ` +
          `${final.reason ?? "no reason stated"}`,
    crashed: false
  } satisfies CellOutcome
})

/**
 * The verdict that blocks a lane, or `undefined` where the lane may run. A lane
 * the document does not mention is NOT blocked: absence means "none" (§2.2), and
 * the driver skips only what the document positively refuses.
 */
const laneBlock = (pivot: Pivot.PivotV3, lane: string): Tokens.LaneVerdict | undefined => {
  const verdict = pivot.case.lanes[lane]
  if (verdict === undefined) return undefined
  return Tokens.parseLaneVerdict(verdict)._tag === "blocked" ? verdict : undefined
}

/**
 * The post-run confrontation of one replay cell: probes off the bundle,
 * classification through the {@link Classifier} seam, and the two record files
 * written at the cell root — after the run, never before, because the
 * interpreter wipes that directory.
 */
const confrontCell = Effect.fn("Driver.confrontCell")(function* (
  cell: Campaign.PivotReplayCell,
  context: CellContext,
  absolute: string,
  pivot: Pivot.PivotV3,
  verdict: Bundle.RunVerdict,
  caseId: string
) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const classifier = yield* Classifier.Service

  const recordings = yield* readRecordings(absolute)
  const flows =
    cell.flows === undefined
      ? undefined
      : yield* Flows.parseFlows(yield* fs.readFileString(cell.flows))
  const resources = yield* readExpectedBodies(path.dirname(yield* documentPath(cell.case)), pivot)
  const confronted = Confront.confront({
    pivot,
    verdict,
    recordings,
    resources,
    ...(flows === undefined ? {} : { flows })
  })

  const meta = {
    lane: cell.lane,
    capture: pivot.case.source?.capture ?? "",
    case: caseId,
    run: 0
  }
  const classify = classifier.classifierFor(cell.lane, confronted.context)
  const records = confronted.probes.map((at) => Confront.recordOf(meta, at, classify(at.probe)))
  yield* fs.writeFileString(
    path.join(absolute, CONFRONTATION_FILE),
    records.map((r) => `${Confrontation.emitConfrontationRecord(r)}\n`).join("")
  )

  const of = (cls: Confrontation.RecordClass): number =>
    records.filter((r) => r.class === cls).length
  const summary: Confrontation.ClassificationSummary = {
    case: caseId,
    lane: cell.lane,
    mode: context.mode,
    records: records.length,
    accepted: of("accepted"),
    known_bug: of("known-bug"),
    unlisted: of("unlisted"),
    unknown: of("unknown"),
    compared: confronted.compared,
    unreferenced: confronted.unreferenced,
    passed: Confrontation.recordsPass(records)
  }
  yield* fs.writeFileString(
    path.join(absolute, CLASSIFICATION_FILE),
    Confrontation.emitClassificationSummary(summary)
  )
  return summary
})

/**
 * Every resource an `expect` step's body references, read from the case
 * directory and keyed by its ref — the texts the confrontation holds the
 * received bodies against.
 */
const readExpectedBodies = Effect.fn("Driver.readExpectedBodies")(function* (
  caseDir: string,
  pivot: Pivot.PivotV3
) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const out = new Map<string, string>()
  for (const step of Pivot.pivotSteps(pivot)) {
    const body = step.msg.body
    if (step.op !== "expect" || body === undefined || !Body.isResourceBody(body)) continue
    if (out.has(body.ref)) continue
    out.set(body.ref, yield* fs.readFileString(path.join(caseDir, body.ref)))
  }
  return out
})

/** Every per-leg recording of one finished bundle, keyed by leg name. */
const readRecordings = Effect.fn("Driver.readRecordings")(function* (absolute: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const dir = path.join(absolute, RECORDING_DIR)
  const out = new Map<string, ReadonlyArray<Bundle.RecordedMessage>>()
  if (!(yield* fs.exists(dir))) return out
  for (const entry of yield* fs.readDirectory(dir)) {
    if (!entry.endsWith(".jsonl")) continue
    const text = yield* fs.readFileString(path.join(dir, entry))
    out.set(entry.slice(0, -".jsonl".length), yield* Bundle.decodeRecording(text))
  }
  return out
})

/** The v3 document a cell names: a file, or `scenario.json` inside a case directory. */
const documentPath = Effect.fn("Driver.documentPath")(function* (target: string) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const info = yield* fs.stat(target)
  return info.type === "Directory" ? path.join(target, "scenario.json") : target
})

/**
 * One crate test. The reporter's `result.json` is authoritative where the crate
 * writes one — it knows what the test asserted — and the exit code is the answer
 * only where no reporter is wired.
 */
const runRustTest = Effect.fn("Driver.runRustTest")(function* (
  cell: Campaign.RustTestCell,
  context: CellContext,
  absolute: string
) {
  const fs = yield* FileSystem.FileSystem
  const path = yield* Path.Path
  const run = yield* toolchainRunner
  const { tests } = context

  // The cell directory is wiped like the interpreter wipes a replay cell's:
  // a stale result.json from an earlier run must never read as this run's.
  yield* fs.remove(absolute, { recursive: true, force: true })
  yield* fs.makeDirectory(absolute, { recursive: true })
  const env = {
    ...(tests.cellDirEnv === undefined ? {} : { [tests.cellDirEnv]: absolute }),
    ...(tests.cellInfraEnv === undefined ? {} : { [tests.cellInfraEnv]: cellIdOf(cell).infra })
  }
  const output = yield* run.all(tests.command, tests.args(cell), {
    ...(tests.cwd === undefined ? {} : { cwd: tests.cwd }),
    ...(Object.keys(env).length === 0 ? {} : { env })
  })

  const resultPath = path.join(absolute, RESULT_FILE)
  if (yield* fs.exists(resultPath)) {
    const result = yield* E2e.decodeRunResult(
      JSON.parse(yield* fs.readFileString(resultPath)) as unknown
    )
    return {
      passed: result.passed,
      detail: result.passed ? "" : failedChecks(result),
      crashed: false
    } satisfies CellOutcome
  }

  // No reporter wrote a record. The exit code is all there is, and a non-zero
  // one with no record is a cell that produced no result — not a red one.
  if (output.exitCode === 0) return { passed: true, detail: "", crashed: false } satisfies CellOutcome
  return {
    passed: false,
    detail:
      `${tests.command} exited ${output.exitCode} and wrote no ${RESULT_FILE}:\n${output.stderr}`,
    crashed: true
  } satisfies CellOutcome
})

/** The checks a red cell failed, in the reporter's own words. */
const failedChecks = (result: E2e.RunResult): string =>
  result.checks
    .filter((c) => !c.passed)
    .map((c) => `${c.on} ${c.field} ${c.op}: ${c.detail}`)
    .join("\n")

const firstLine = (text: string): string => text.split("\n")[0] ?? ""
