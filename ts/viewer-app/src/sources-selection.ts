/**
 * What the Sources page replays: the ticked captures whole, held across
 * pages and filters, or every capture the page shows where none is ticked.
 */
import type { RequestSelection } from "./api.ts"

export const sourcesSelection = (
  ticked: ReadonlySet<string>,
  shown: ReadonlyArray<string>
): RequestSelection | string => {
  const captures = [...new Set(ticked.size === 0 ? shown : ticked)].sort()
  return captures.length === 0 ? "no capture on screen" : { kind: "captures", captures }
}

/** The ticks after the header box is clicked: every shown capture ticked, or all of them unticked when they all were. */
export const tickAllShown = (ticked: ReadonlySet<string>, shown: ReadonlyArray<string>): ReadonlySet<string> => {
  const next = new Set(ticked)
  const all = shown.length > 0 && shown.every((capture) => ticked.has(capture))
  for (const capture of shown) {
    if (all) next.delete(capture)
    else next.add(capture)
  }
  return next
}
