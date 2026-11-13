/**
 * A campaign validated with no lane stood, nothing provisioned and no call
 * placed: the run's own compile phase (`run.ts` `Phases.compile`, the very
 * effect a run runs, so the two cannot disagree on what compiles) into a
 * cells directory, then three levels of problems, each named by its case file
 * and sub-case:
 *
 * - `compile`: a case file the compile names broken (it did not load, its
 *   compiler refused it, a tag it names is undeclared, or an operation step
 *   carries no tag stating alone), or the phase failing (two sub-cases
 *   compiling to one cell id, a kept case no file compiles);
 * - `schema`: a compiled document the pivot schema lint finds an error in
 *   (references, `after` order, steps, checks, postconditions), whether the
 *   compile linted it (its file broken) or not (each compiled cell's);
 * - `consistency`: two cells filed under one case id (ref and sub-case), which
 *   the store keys a cell's results by.
 *
 * What only a run settles is a note, never a problem: a lane timer a cell was
 * compiled with (the run's lanes decide it, a lane stating another skipping
 * the cell), and a cell none of the given lanes runs (a capability it needs,
 * an operation it asks for that the lane binds none of): such a cell is a skip
 * at run, not a failure.
 */
import type { LaneView } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import type { Cell } from "./cell.js"
import type { Lint } from "./lint.js"
import { unfitOf } from "./needs.js"
import { unboundOperationsOf } from "./operation-needs.js"
import { type CompiledCells, PhaseFailed } from "./run.js"
import * as Results from "@sip/store/campaign-results"

/** Where a problem was found: the compile phase, the pivot schema lint, the checks across cells. */
export type Level = "compile" | "schema" | "consistency"

/** One problem of a validation, named by its case file and sub-case (none for a whole file or the phase). */
export interface Problem {
  readonly level: Level
  readonly file: string
  readonly subcase: string | null
  readonly message: string
}

/** What a validation came to: the cells compiled, the problems found, what is settled only at run. */
export interface Validation<C extends Cell = Cell> {
  readonly cells: ReadonlyArray<C>
  readonly problems: ReadonlyArray<Problem>
  readonly notes: ReadonlyArray<string>
}

/** What a validation reads beside the compile phase. */
export interface Options {
  /** The lint every compiled document's `scenario.json` passes. */
  readonly lint: Lint
  /** The lanes a run of the campaign may run on: a cell none of them runs is noted. */
  readonly lanes: ReadonlyArray<LaneView.View>
}

/** A cell's name in a problem or a note: its case file and its case id. */
const named = (cell: Cell): string => `${cell.source.file}: ${caseIdOf(cell)}`

/** The case id the store files `cell`'s results under: its ref, with its sub-case where it states one. */
const caseIdOf = (cell: Cell): string => Results.caseIdOf(cell.ref, cell.subcase === "" ? undefined : cell.subcase)

/** The schema problems of `cell`'s written document, or one naming why the lint did not run. */
const schemaProblemsOf = (lint: Lint, cell: Cell) =>
  lint(path.join(cell.dir, "scenario.json")).pipe(
    Effect.map((findings) => findings.map((message) => ({ message }))),
    Effect.catch((e) => Effect.succeed([{ message: `the pivot schema lint did not run: ${e.message}` }])),
    Effect.map((found) =>
      found.map(({ message }): Problem => ({ level: "schema", file: cell.source.file, subcase: cell.subcase, message }))
    )
  )

/** Two cells or more filed under one case id: one problem per cell after the first, naming the first. */
export const sharedCaseIdsOf = (cells: ReadonlyArray<Cell>): ReadonlyArray<Problem> => {
  const first = new Map<string, Cell>()
  const problems: Array<Problem> = []
  for (const cell of cells) {
    const id = caseIdOf(cell)
    const taken = first.get(id)
    if (taken === undefined) {
      first.set(id, cell)
      continue
    }
    problems.push({
      level: "consistency",
      file: cell.source.file,
      subcase: cell.subcase,
      message: `the cell ${cell.id} is filed under the case id ${id}, as the cell ${taken.id} of ${taken.source.file} is: ` +
        "the store keys a cell's results by it"
    })
  }
  return problems
}

/** What only a run settles of `cell` on `lanes`: its lane timers, and why no lane runs it where none does. */
export const notesOf = (lanes: ReadonlyArray<LaneView.View>, cell: Cell): ReadonlyArray<string> => {
  const timers = Object.entries(cell.laneTimers ?? {}).map(([name, ms]) =>
    `${named(cell)}: lane timer ${name} compiled as ${ms} ms, resolved at run over the run's lanes`
  )
  const why = lanes.map((lane) => unboundOperationsOf(lane, cell) ?? unfitOf(lane, cell, false))
  const nowhere = lanes.length > 0 && why.every((reason) => reason !== undefined)
    ? [`${named(cell)}: runs on no lane, skipped at run: ${why.join("; ")}`]
    : []
  return [...timers, ...nowhere]
}

/** The phase's failure as the one compile problem of the validation. */
const phaseProblem = (failed: PhaseFailed): Problem => ({ level: "compile", file: "", subcase: null, message: failed.message })

/** The validation of what `compile` (the run's compile phase) compiles, over `options`. */
export const validate = <C extends Cell, R>(compile: Effect.Effect<CompiledCells<C>, PhaseFailed, R>, options: Options) =>
  Effect.gen(function*() {
    const compiled = yield* Effect.result(compile)
    if (compiled._tag === "Failure") {
      return { cells: [], problems: [phaseProblem(compiled.failure)], notes: [] } satisfies Validation<C>
    }
    const { broken, cells } = compiled.success
    const problems: Array<Problem> = broken.map((file) => ({
      level: file.lint === true ? "schema" : "compile",
      file: file.file,
      subcase: null,
      message: file.reason
    }))
    for (const cell of cells) problems.push(...yield* schemaProblemsOf(options.lint, cell))
    problems.push(...sharedCaseIdsOf(cells))
    if (cells.length === 0 && broken.length === 0) {
      problems.push({ level: "compile", file: "", subcase: null, message: "no case file compiled to a cell" })
    }
    return { cells, problems, notes: cells.flatMap((cell) => notesOf(options.lanes, cell)) } satisfies Validation<C>
  })

/** One line per problem, `<level> <file> [<sub-case>]: <message>`. */
export const problemLines = (validation: Validation): ReadonlyArray<string> =>
  validation.problems.map((p) =>
    `${p.level} ${p.file === "" ? "(the compile phase)" : p.file}${p.subcase === null || p.subcase === "" ? "" : ` [${p.subcase}]`}: ${p.message}`
  )

/** A validation's exit code: 0 where it found no problem, else 1. */
export const exitOf = (validation: Validation): 0 | 1 => (validation.problems.length === 0 ? 0 : 1)

/** What a cell description holds of a cell: its runner fields, its case file named and its text left out. */
export const describedOf = (cell: Cell) => {
  const { source, ...rest } = cell
  return { ...rest, file: source.file }
}

/** What a validation writes under its `--out`: anything else there is not its own. */
const WRITTEN = new Set(["cells", "cells.json", "validation.json"])

/**
 * Why `out` cannot take a validation's output, or `undefined`: it holds an
 * entry a validation does not write. An absent or empty directory, or one a
 * validation wrote, takes it.
 */
export const outRefusal = (out: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const entries = yield* fs.readDirectory(out).pipe(Effect.orElseSucceed((): ReadonlyArray<string> => []))
    const foreign = entries.filter((entry) => !WRITTEN.has(entry)).sort()
    return foreign.length === 0
      ? undefined
      : `--out ${out} holds ${foreign.join(", ")}, which a validation does not write: name an empty directory, or one a validation wrote`
  })

/**
 * `body` over a cells directory: `<out>/cells` where `out` is named (the
 * compiled documents kept for inspection), emptied first so it holds this
 * validation's cells alone, else a scratch directory removed once `body` ends.
 * The caller has had `out` pass {@link outRefusal}.
 */
export const withCellsDir = <A, E, R>(out: string | undefined, body: (cellsDir: string) => Effect.Effect<A, E, R>) =>
  Effect.scoped(Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    if (out === undefined) return yield* body(yield* fs.makeTempDirectoryScoped({ prefix: "campaign-validate-" }).pipe(Effect.orDie))
    const cellsDir = path.join(path.resolve(out), "cells")
    yield* fs.remove(cellsDir, { recursive: true, force: true }).pipe(Effect.orDie)
    yield* fs.makeDirectory(cellsDir, { recursive: true }).pipe(Effect.orDie)
    return yield* body(cellsDir)
  }))

/**
 * `validation` written under `out`: `cells.json`, each cell's description in
 * compile order, and `validation.json`, its problems and notes; with `expand`,
 * each cell's `expanded.json` beside its document, `expand` making it.
 */
export const writeOut = <C extends Cell, R>(
  out: string,
  validation: Validation<C>,
  expand?: (cell: C) => Effect.Effect<unknown, never, R>
) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const json = (value: unknown) => `${JSON.stringify(value, null, 2)}\n`
    yield* fs.makeDirectory(out, { recursive: true })
    yield* fs.writeFileString(path.join(out, "cells.json"), json(validation.cells.map(describedOf)))
    yield* fs.writeFileString(path.join(out, "validation.json"), json({ problems: validation.problems, notes: validation.notes }))
    if (expand === undefined) return
    for (const cell of validation.cells) {
      yield* fs.writeFileString(path.join(cell.dir, "expanded.json"), json(yield* expand(cell)))
    }
  })
