/**
 * The replay summary: one line per cell the invocation ran — how it ended,
 * its first failing step and main error — beside the call's current run (the
 * current population's, on the lane), failures first, under one line counting
 * what moved against it. Read off the store, so it is the same reading a
 * comparison makes of the two sets.
 */
import { Compare, RunSets, Selector, StoreBinding } from "@sip/store"
import * as Effect from "effect/Effect"
import { errorLine, outcomeOf } from "./compare-report.js"

const keyOf = (cell: Pick<Compare.Cell, "capture" | "caseId">): string => `${cell.capture}/${cell.caseId}`

/** Failures first, then passes, then the cells a lane block kept from running; input order within each. */
const rank = (cell: Compare.Cell): number => (cell.skipped !== null ? 2 : cell.passed ? 1 : 0)

const said = (cell: Compare.Cell): string => `${outcomeOf(cell)}${errorLine(cell)}`

/** The summary of `own` (this invocation's cells) against `current` (keyed `capture/case`), labelled `label`. */
export const lines = (
  own: ReadonlyArray<Compare.Cell>,
  current: ReadonlyMap<string, Compare.Cell>,
  label: string
): ReadonlyArray<string> => {
  const skipped = own.filter((cell) => cell.skipped !== null).length
  const failed = own.filter((cell) => cell.skipped === null && !cell.passed).length
  const passed = own.length - skipped - failed
  let moved = 0
  let unchanged = 0
  let never = 0
  for (const cell of own) {
    const then = current.get(keyOf(cell))
    if (then === undefined) never += 1
    else if (Compare.classOf(then, cell) === "unchanged") unchanged += 1
    else moved += 1
  }
  const ordered = own.map((cell, at) => ({ cell, at })).sort((a, b) => rank(a.cell) - rank(b.cell) || a.at - b.at)
  return [
    `[summary] ${own.length} cell(s): ${failed} failed, ${passed} passed${skipped === 0 ? "" : `, ${skipped} skipped`}; ` +
    `against ${label}: ${moved} moved, ${unchanged} unchanged, ${never} never ran there`,
    ...ordered.map(({ cell }) => {
      const then = current.get(keyOf(cell))
      return `  ${cell.caseId}  ${said(cell)}  run${cell.runId}  |  ` +
        (then === undefined ? `${label}: never ran` : `${label} run${then.runId} ${said(then)}`)
    })
  ]
}

/**
 * The summary of the runs `ran` names (the invocation's sweep or job) against
 * the current population's runs of the same calls on `lane`.
 */
export const read = Effect.fn("ReplaySummary.read")(function*(ran: string, lane: string) {
  const sets = yield* RunSets.Service
  const label = yield* StoreBinding.current
  const own = yield* StoreBinding.parse(ran)
  const current = yield* StoreBinding.parse(label)
  if (Selector.isRefused(own)) return [`[summary] ${own.refused}`]
  if (Selector.isRefused(current)) return [`[summary] ${current.refused}`]
  const mine = yield* sets.pair(own, own, 0)
  const against = yield* sets.pair(own, Selector.onLane(current, lane), 0)
  return lines(mine.after.cells, new Map(against.after.cells.map((cell) => [keyOf(cell), cell])), label)
})
