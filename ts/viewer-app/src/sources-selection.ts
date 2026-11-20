/**
 * What the Sources page replays: the ticked captures whole, held across
 * pages and filters, or every capture the page shows where none is ticked.
 * Only a capture whose cut holds cases is replayable, its cut current or
 * stale; the others are left out and named, since one capture no cut holds
 * refuses the whole request.
 */
import type { RequestSelection, SourceRow } from "./api.ts"
import { wholeCaptures } from "./request-builder.ts"

export interface SourcesSelection {
  readonly selection: RequestSelection | string
  /** The captures asked for and left out: no cut, a broken one, or no case. */
  readonly left: readonly string[]
}

const replayable = (row: SourceRow): boolean => (row.cut === "current" || row.cut === "stale") && row.cases > 0

export const sourcesSelection = (
  ticked: ReadonlyMap<string, SourceRow>,
  shown: ReadonlyArray<SourceRow>
): SourcesSelection => {
  const rows = ticked.size === 0 ? shown : [...ticked.values()]
  const left = [...new Set(rows.filter((row) => !replayable(row)).map((row) => row.capture))].sort()
  const selection = wholeCaptures(rows.filter(replayable).map((row) => row.capture))
  return {
    selection: typeof selection === "string" && left.length > 0 ? "no capture asked for has a cut with cases" : selection,
    left
  }
}

/** The ticks after the header box is clicked: every shown capture ticked, or all of them unticked when they all were. */
export const tickAllShown = (
  ticked: ReadonlyMap<string, SourceRow>,
  shown: ReadonlyArray<SourceRow>
): ReadonlyMap<string, SourceRow> => {
  const next = new Map(ticked)
  const all = shown.length > 0 && shown.every((row) => ticked.has(row.capture))
  for (const row of shown) {
    if (all) next.delete(row.capture)
    else next.set(row.capture, row)
  }
  return next
}
