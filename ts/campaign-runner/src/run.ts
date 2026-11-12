/**
 * The phases of one campaign run, in their order: the compile of the case files
 * into cells (a case asked for that no file compiles fails here, before the
 * database is touched), one provisioning phase before any call, the setup
 * operations injected, a replay batch per lane inside the job's collectors
 * (opened before the first, the database's dump among them; collected and
 * closed after the last, their answers folded into the cells), the report,
 * then the setup healed, whatever ended the run (`setup-phase.ts`).
 * The exit code is 2 when anything errored (a phase, a cell no call was
 * judged in, a report that was not written, a setup operation or a job
 * collector that left the lab changed, or a line of the job's collectors that
 * did not pass), else 1 when a cell failed, else 0.
 * A setup whose inject fails heals, then reports the error: no call ran.
 *
 * A case file the compile phase names broken is an error of the run, the
 * other files' cells still running. A compile or provisioning phase that
 * fails ends the run there. A lane whose
 * replay could not run errors each of its cells and the next lane runs; a
 * batch whose own end gate found the SUT dirty is an error of the run.
 *
 * After a lane's batch, each cell that ended `failure` and declares retries
 * is replayed again on that lane, in a batch of its own with the other cells
 * being retried, until an attempt ends other than `failure` or its count runs
 * out; each attempt's batch is judged as the first one is. The lane's verdict
 * for the cell is its best attempt ({@link bestOf}); once the job's
 * collectors answered, each lane's kept attempts go to {@link Phases.laneKept}.
 *
 * The phases are the pack's, over its own cells; the checks it types itself
 * (`check-lines.ts` `Own`) read each cell as the store holds it.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { storedOutcomeOf } from "./case-result.js"
import { type Cell, type CellResult, errored, type Lane } from "./cell.js"
import type * as CheckLines from "./check-lines.js"

/** A selected case file the compile could not use, and why. */
export interface BrokenFile {
  readonly file: string
  readonly reason: string
}

/** The cells compiled, and the selected files that are broken. */
export interface CompiledCells<C extends Cell = Cell> {
  readonly cells: ReadonlyArray<C>
  readonly broken: ReadonlyArray<BrokenFile>
}

export class PhaseFailed extends Schema.TaggedError<PhaseFailed>()("CampaignRunner.PhaseFailed", {
  phase: Schema.String,
  reason: Schema.String
}) {
  override get message(): string {
    return `${this.phase}: ${this.reason}`
  }
}

/** A phase that did not run to its end. */
export interface PhaseError {
  readonly phase: string
  readonly message: string
}

export interface Outcome<C extends Cell = Cell> {
  readonly results: ReadonlyArray<CellResult<C>>
  readonly errors: ReadonlyArray<PhaseError>
}

/** One lane's replay: each cell's result, and what the batch's end left dirty that no cell was charged with. */
export interface LaneOutcome<C extends Cell = Cell> {
  readonly results: ReadonlyArray<CellResult<C>>
  /** What the batch's end gate found, how its process died, which collector's close left the lab changed. */
  readonly dirty?: string
}

export interface Phases<R, C extends Cell = Cell> {
  readonly provision: Effect.Effect<void, PhaseFailed, R>
  /** `body`, the rest of the run, inside the setup operations: injected before it, healed after it however it ends. */
  readonly setup: <A, RB>(body: Effect.Effect<A, never, RB>) => Effect.Effect<{ readonly value: A; readonly labLeftChanged: boolean }, PhaseFailed, R | RB>
  /**
   * `replays` of `cells`, every lane's kept results, inside the job's
   * collectors: opened before them, `cells` planned, collected over their
   * cells and closed once they ended; each result with what they answered of
   * its cell, and who left the lab changed.
   */
  readonly collectors: <RB>(
    cells: ReadonlyArray<C>,
    replays: Effect.Effect<ReadonlyArray<CellResult<C>>, never, RB>
  ) => Effect.Effect<
    {
      readonly results: ReadonlyArray<CellResult<C>>
      readonly leftChanged: ReadonlyArray<string>
      /** The job's own lines that decide and did not pass, each an error of the run. */
      readonly unanswered: ReadonlyArray<string>
    },
    never,
    R | RB
  >
  readonly compile: Effect.Effect<CompiledCells<C>, PhaseFailed, R>
  /** One batch of `cells` on `lane`: the lane's first (`attempt` 1), or a retry of its failed cells. */
  readonly replay: (lane: Lane, cells: ReadonlyArray<C>, attempt: number) => Effect.Effect<LaneOutcome<C>, PhaseFailed, R>
  /** The lane's kept attempt of each cell, once the job's collectors answered: what is drawn over the lane's verdicts. */
  readonly laneKept: (lane: Lane, kept: ReadonlyArray<CellResult<C>>) => Effect.Effect<void, never, R>
  readonly report: (outcome: Outcome<C>) => Effect.Effect<void, PhaseFailed, R>
}

export type ExitCode = 0 | 1 | 2

/**
 * The run's exit code from what it came to, each cell read as its stored
 * lines read it over `own` (`case-result.ts` {@link storedOutcomeOf}): 2 for a
 * phase error or a cell in error (an unverified one included), else 1 for a
 * failed cell.
 */
export const exitCode = (own: CheckLines.Own, outcome: Outcome): ExitCode => {
  const outcomes = outcome.results.map((result) => storedOutcomeOf(own, result))
  return outcome.errors.length > 0 || outcomes.includes("error") ? 2 : outcomes.includes("failure") ? 1 : 0
}

const errorOf = (failed: PhaseFailed): PhaseError => ({ phase: failed.phase, message: failed.reason })

/**
 * The index of the best of a cell's attempts on a lane: the first that passed,
 * a skipped (not-checkable) attempt counting as a pass, else the failure with
 * the fewest failed checks, the earliest on a tie; an errored attempt only
 * where no attempt is better.
 */
export const bestOf = (attempts: ReadonlyArray<CellResult<Cell>>): number => {
  const clean = attempts.findIndex((a) => a.status === "pass" || a.status === "skipped")
  if (clean >= 0) return clean
  let best = -1
  for (const [i, a] of attempts.entries()) {
    if (a.status !== "failure") continue
    if (best < 0 || a.checks.length < attempts[best]!.checks.length) best = i
  }
  return best < 0 ? 0 : best
}

/** The lane's verdict for a cell from its attempts: the best one, carrying every attempt where there were several. */
const keptOf = <C extends Cell>(attempts: ReadonlyArray<CellResult<C>>): CellResult<C> => {
  const kept = bestOf(attempts)
  const result = attempts[kept]!
  if (attempts.length === 1) return result
  return {
    ...result,
    attempts: attempts.map((a, i) => ({
      ...(a.bundle === undefined ? {} : { bundle: a.bundle }),
      status: a.status,
      failed: a.checks.length,
      kept: i === kept
    }))
  }
}

/** Whether `result`, its cell's `tried`-th attempt, is replayed again. */
const retried = (result: CellResult<Cell>, tried: number): boolean => result.status === "failure" && tried <= result.cell.retries

/**
 * Each lane's batches of `cells`, its retries included: each cell's kept
 * attempt answered, what errored outside a cell to `errors`.
 */
const replayLanes = <R, C extends Cell>(
  lanes: ReadonlyArray<Lane>,
  cells: ReadonlyArray<C>,
  phases: Phases<R, C>,
  errors: Array<PhaseError>
) =>
  Effect.gen(function*() {
    const results: Array<CellResult<C>> = []
    for (const lane of lanes) {
      /** Each cell's attempts on the lane, in the order the first batch ran them. */
      const attempts = new Map<C, Array<CellResult<C>>>(cells.map((cell) => [cell, []]))
      let batch: ReadonlyArray<C> = cells
      for (let attempt = 1; batch.length > 0; attempt++) {
        const said = attempt === 1 ? "" : `attempt ${attempt}: `
        const replayed = yield* Effect.result(phases.replay(lane, batch, attempt))
        if (replayed._tag === "Failure") {
          if (attempt > 1) errors.push({ phase: `replay ${lane}`, message: `${said}${replayed.failure.reason}` })
          for (const cell of batch) attempts.get(cell)?.push(errored(lane, cell, replayed.failure.reason))
          break
        }
        const outcome = replayed.success
        for (const result of outcome.results) attempts.get(result.cell)?.push(result)
        if (outcome.dirty !== undefined) {
          errors.push({ phase: `replay ${lane}`, message: `${said}batch end: ${outcome.dirty}` })
        }
        batch = outcome.results.filter((r) => retried(r, attempt)).map((r) => r.cell)
      }
      results.push(...[...attempts.values()].filter((tried) => tried.length > 0).map(keptOf))
    }
    return results as ReadonlyArray<CellResult<C>>
  })

/** The run of `phases` on `lanes`, its exit code read over the checks the pack types itself (`own`). */
export const run = <R, C extends Cell>(
  lanes: ReadonlyArray<Lane>,
  phases: Phases<R, C>,
  own: CheckLines.Own
): Effect.Effect<ExitCode, never, R> =>
  Effect.gen(function*() {
    const errors: Array<PhaseError> = []
    const results: Array<CellResult<C>> = []
    const finish = Effect.suspend(() => {
      const outcome = { results, errors }
      return Effect.match(phases.report(outcome), {
        onFailure: (): ExitCode => 2,
        onSuccess: () => exitCode(own, outcome)
      })
    })
    const compiled = yield* Effect.result(phases.compile)
    if (compiled._tag === "Failure") {
      errors.push(errorOf(compiled.failure))
      return yield* finish
    }
    const { cells, broken } = compiled.success
    for (const file of broken) errors.push({ phase: `compile ${file.file}`, message: file.reason })
    if (cells.length === 0) {
      errors.push({ phase: "compile", message: "no case file compiled to a cell" })
      return yield* finish
    }
    const provisioned = yield* Effect.result(phases.provision)
    if (provisioned._tag === "Failure") {
      errors.push(errorOf(provisioned.failure))
      return yield* finish
    }
    const healed = yield* Effect.result(phases.setup(Effect.gen(function*() {
      const collected = yield* phases.collectors(cells, replayLanes(lanes, cells, phases, errors))
      results.push(...collected.results)
      for (const lane of lanes) yield* phases.laneKept(lane, collected.results.filter((result) => result.lane === lane))
      for (const label of collected.leftChanged) errors.push({ phase: "collectors", message: `the collector ${label} left the lab changed` })
      for (const line of collected.unanswered) errors.push({ phase: "collectors", message: line })
      return yield* finish
    })))
    if (healed._tag === "Failure") {
      errors.push(errorOf(healed.failure))
      return yield* finish
    }
    return healed.success.labLeftChanged ? 2 : healed.success.value
  })
