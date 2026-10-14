/**
 * What a batch's gate read of a standing SUT's call contexts, kept on every
 * run the batch's cells ran under: `<run>/contexts.ndjson`, one line per
 * batch. A held context one cell's call left is that cell's: named on its
 * run by the cell's directory, and charged to that run (the cell's verdict
 * already states it). One no single cell's call names is the batch's, named
 * on every run the batch touched, and is the batch's dirty finding.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import type { ContextsReport } from "./batch-protocol.js"

/** The run file the lines are appended to. */
export const FILE = "contexts.ndjson"

/** One held context as a run keeps it. */
export interface HeldLine {
  readonly ref_id: string
  readonly call_ids: ReadonlyArray<string>
  /** The cell directories under this run whose call it serves; [] where no cell of this run's does. */
  readonly cells: ReadonlyArray<string>
  /** Whether it is the one cell's charge (exactly one cell of the batch named it). */
  readonly charged: boolean
}

/** One batch's reading, as a run keeps it. */
export interface Line {
  /** When the batch ended. */
  readonly at: string
  readonly store: string
  readonly before?: number
  readonly after?: number
  readonly unread?: string
  readonly held: ReadonlyArray<HeldLine>
}

const under = (runDir: string, cell: string): boolean => path.dirname(path.resolve(cell)) === path.resolve(runDir)

/**
 * The reading `report` as the run at `runDir` keeps it: the contexts its own
 * cells' calls left, and every one that is the batch's.
 */
export const lineFor = (report: ContextsReport, runDir: string, at: string): Line => {
  const held = report.held.flatMap((one): ReadonlyArray<HeldLine> => {
    const charged = one.cells.length === 1
    const mine = one.cells.filter((cell) => under(runDir, cell)).map((cell) => path.basename(cell))
    if (charged && mine.length === 0) return []
    return [{ ref_id: one.ref_id, call_ids: one.call_ids, cells: mine, charged }]
  })
  return {
    at,
    store: report.store,
    ...(report.before === undefined ? {} : { before: report.before }),
    ...(report.after === undefined ? {} : { after: report.after }),
    ...(report.unread === undefined ? {} : { unread: report.unread }),
    held
  }
}

/**
 * Each held context one cell's call left, as its run's cleanup finding: the
 * run the cell is under, of `runDirs`, the cell's directory under it, and the
 * words the row keeps.
 */
export const charges = (
  report: ContextsReport,
  runDirs: ReadonlyArray<string>
): ReadonlyArray<{ readonly runDir: string; readonly cell: string; readonly finding: string }> =>
  report.held.flatMap((one) => {
    if (one.cells.length !== 1) return []
    const cell = one.cells[0]!
    const runDir = runDirs.find((dir) => under(dir, cell))
    if (runDir === undefined) return []
    return [{
      runDir,
      cell: path.basename(cell),
      finding: `cell ${path.basename(cell)}: the SUT at ${report.store} still holds call context ` +
        `${one.ref_id} of its call after its batch (expected none)`
    }]
  })

/** Appends the batch's reading to every run in `runDirs`; a run it cannot write to loses the line, not the batch. */
export const keep = (report: ContextsReport, runDirs: ReadonlyArray<string>, at: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    for (const runDir of new Set(runDirs)) {
      yield* Effect.gen(function*() {
        yield* fs.makeDirectory(runDir, { recursive: true })
        yield* fs.writeFileString(path.join(runDir, FILE), `${JSON.stringify(lineFor(report, runDir, at))}\n`, {
          flag: "a"
        })
      }).pipe(
        Effect.catch((error) => Console.error(`[replay] ${runDir}: the contexts reading was not kept: ${error.message}`))
      )
    }
  })
