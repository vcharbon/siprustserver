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
 * ran. A cell that ran its interpreter and did not pass keeps what the
 * interpreter wrote on stderr in `stderr.txt`, whatever the verdict says. A
 * pivot-replay cell whose lane still waits on one of its verification lines
 * (`pending` in `verifications.json`) lands not passed, naming them: a line
 * stays pending until its batch's end answers it, and the cell is read again
 * then (`pending.ts` `settled`). A cell whose document declares its lane
 * BLOCKED is not red either: it leaves a `skipped.json` and never reaches the
 * interpreter.
 *
 * A cell is INTERRUPTIBLE: an interruption reaches the interpreter's process
 * (SIGTERM, then SIGKILL after the toolchain's grace), and the cell leaves
 * `error.txt` saying `interrupted` where its bundle would have been. It
 * answers nothing — its fiber is interrupted — so the caller counts it from
 * what it admitted and what landed.
 */
import { Bundle, Campaign, CellHits, Confrontation, E2e, Pivot, Tokens, Verification } from "@sip/contracts"
import { Classifier, Confront, Reclassifier } from "@sip/pipeline"
import { ReplayCli, runner as toolchainRunner } from "@sip/toolchain"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Path from "effect/Path"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import { LanePresets } from "./lanes.js"
import { confrontInputOf, documentPath } from "./confront-input.js"
import { landedIn } from "./pending.js"
import {
  cellDir,
  cellIdOf,
  CLASSIFICATION_FILE,
  CONFRONTATION_FILE,
  ERROR_FILE,
  RESULT_FILE,
  STDERR_FILE,
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
  /**
   * Why this host runs no crate test, in the words a reader is shown; set,
   * every crate-test cell is an error carrying it and nothing is spawned.
   */
  readonly unavailable?: string
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

/** What an interrupted cell leaves in its `error.txt`. */
export const INTERRUPTED = "interrupted"

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
  const outcome = yield* guarded(body).pipe(
    Effect.onInterrupt(() =>
      Effect.ignore(
        Effect.andThen(
          fs.makeDirectory(absolute, { recursive: true }),
          fs.writeFileString(path.join(absolute, ERROR_FILE), `${INTERRUPTED}\n`)
        )
      )
    )
  )

  if (outcome.crashed) {
    yield* fs.makeDirectory(absolute, { recursive: true })
    yield* fs.writeFileString(path.join(absolute, ERROR_FILE), `${outcome.detail}\n`)
  }
  if (!outcome.passed && outcome.stderr !== undefined && outcome.stderr !== "") {
    yield* fs.makeDirectory(absolute, { recursive: true })
    yield* fs.writeFileString(path.join(absolute, STDERR_FILE), outcome.stderr)
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
      ...(outcome.skipped === undefined ? {} : { skipped: outcome.skipped }),
      ...(outcome.sourceFault === undefined ? {} : { sourceFault: outcome.sourceFault }),
      ...(outcome.restated === undefined ? {} : { restated: outcome.restated }),
      ...(outcome.restatedLines === undefined ? {} : { restatedLines: outcome.restatedLines }),
      ...(outcome.pending === undefined ? {} : { pending: outcome.pending })
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
  /** What the interpreter wrote on stderr, where the cell ran one; kept beside a cell that did not pass. */
  readonly stderr?: string
  /**
   * The lane verdict that kept the cell from running; absent where it ran. A
   * skipped cell is the third outcome: `passed` is false because nothing passed,
   * and no campaign fails because of it.
   */
  readonly skipped?: Tokens.LaneVerdict
  /** The cell's RFC audit holds a source fault: information beside `passed`, which it never decides. */
  readonly sourceFault?: true
  /** The reclassifier restated the failed run as passed: its reason. */
  readonly restated?: string
  /** Beside `restated`: the lane's lines failing the cell as it landed, the ones restated. */
  readonly restatedLines?: ReadonlyArray<string>
  /** The lines the lane still waits on, where only they keep the cell from passing. */
  readonly pending?: ReadonlyArray<string>
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
 * the run. A cell that only lines its lane still waits on keep from passing
 * lands not passed, naming them (`pending.ts`). A cell the reclassifier
 * restates as passed names the lane's lines failing it as it lands: those,
 * and no line failing later, are what it restated.
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
      crashed: true,
      stderr: report.stderr
    } satisfies CellOutcome
  }

  const verdictPath = path.join(absolute, VERDICT_FILE)
  const verdict = yield* Bundle.decodeRunVerdict(
    JSON.parse(yield* fs.readFileString(verdictPath)) as unknown
  )
  // The lane's post-run RFC audit is the one instrument that sees a SUT
  // violation the scripted peers answer without complaint: a gating finding
  // fails the cell exactly as a failed verdict does. A scripted party's
  // findings are information, a source fault included: the cell is judged on
  // the SUT alone, and the source fault is named beside that verdict. A bundle
  // without the file was not audited, and says nothing.
  const rfcPath = path.join(absolute, RFC_FILE)
  const audit = (yield* fs.exists(rfcPath))
    ? yield* Bundle.decodeRunRfcAudit(JSON.parse(yield* fs.readFileString(rfcPath)) as unknown)
    : undefined
  const rfcGating = audit === undefined ? [] : Bundle.rfcGating(audit)
  const source = audit === undefined ? [] : Bundle.rfcSourceFaults(audit)
  // A run whose body panicked never passes, and no restatement makes it pass:
  // whatever verdict it left was written before the unwind, so it cannot
  // answer for what came after.
  const panicked = report.outcome._tag === "panicked"
  const landed = yield* landedIn(absolute)
  const pending = landed.pending
  const structural = !panicked && Bundle.verdictPassed(verdict) && rfcGating.length === 0

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
  const sutPassed = !panicked && final.passed
  const awaiting = sutPassed && pending.length > 0
  const restated = sutPassed && !gated ? final.reason ?? "no reason stated" : undefined
  const sourceSaid = `(see ${RFC_FILE}):\n` +
    source.map((f) => `  ${f.rule} [${f.lane}] by ${f.actor ?? "a scripted party"}: ${f.detail}`).join("\n")
  const sutSaid = panicked
    ? `replay exited ${report.exitCode} (panicked):\n${report.stderr}`
    : awaiting
    ? `still verifying: ${pending.join(", ")} pending (see ${Verification.FILE})`
    : final.passed === gated
      ? gated === structural
        ? rfcGating.length > 0
          ? `the RFC audit failed the cell (see ${RFC_FILE}):\n` +
            rfcGating.map((f) => `  ${f.rule} [${f.lane}]: ${f.detail}`).join("\n")
          : report.stderr
        : `classification failed the cell: ${classification.unlisted} unlisted, ` +
          `${classification.unknown} unknown (see ${CONFRONTATION_FILE})`
      : `${verdict.status} restated as ${final.passed ? "passed" : "failed"}: ` +
        `${final.reason ?? "no reason stated"}`
  return {
    passed: sutPassed && !awaiting,
    detail: source.length === 0
      ? sutSaid
      : sutPassed
      ? `a source fault stands as information: the replayed party repeats a captured message where, in this ` +
        `run, the RFC no longer allows it, and the document does not state it ${sourceSaid}`
      : `${sutSaid}\nbeside it, a source fault ${sourceSaid}`,
    crashed: false,
    stderr: report.stderr,
    ...(source.length > 0 ? { sourceFault: true as const } : {}),
    ...(restated === undefined ? {} : { restated, restatedLines: landed.failing }),
    ...(awaiting ? { pending } : {})
  } satisfies CellOutcome
})

/**
 * The verdict that blocks a lane, or `undefined` where the lane may run: the
 * lane's own verdict, else the one stated for every lane (`ANY_LANE`). A lane
 * the document mentions neither way is NOT blocked: absence means "none"
 * (§2.2), and the driver skips only what the document positively refuses.
 */
export const laneBlock = (pivot: Pivot.PivotV3, lane: string): Tokens.LaneVerdict | undefined => {
  const verdict = pivot.case.lanes[lane] ?? pivot.case.lanes[LanePresets.ANY_LANE]
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

  const confronted = Confront.confront({ ...(yield* confrontInputOf(cell, absolute, pivot, verdict)), packOf: classifier.packOf })

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
  if (tests.unavailable !== undefined) {
    return { passed: false, detail: tests.unavailable, crashed: true } satisfies CellOutcome
  }
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
