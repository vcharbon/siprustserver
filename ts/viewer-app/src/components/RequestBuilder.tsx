/**
 * The replay control of the queue (§5 R1, R2, R6, R9): the selection's
 * modes — the filter bar as it stands on the page's lane (read there whatever
 * lanes are picked), the ticked rows, those rows' captures whole, or the
 * population under its predicates — around the `ReplayLauncher` that
 * previews, creates and starts it.
 */
import type { JSX } from "react"
import { useState } from "react"
import type { TriageCallRow } from "../api.ts"
import { type Mode, NO_PREDICATES, type Predicates, selectionOf } from "../request-builder.ts"
import type { ChipFilter } from "../queue-filter.ts"
import { ReplayLauncher } from "./ReplayLauncher.tsx"

const MODES: ReadonlyArray<readonly [Mode, string]> = [
  ["filter", "the current filter"],
  ["ticked", "the ticked rows"],
  ["captures", "these captures whole"],
  ["population", "the population"]
]

export const RequestBuilder = (
  { lane, lanes, scope, q, filter, ticked, shown }: {
    /** The lane the queue is read on: what a filter selection is read on, and the lane picked by default. */
    readonly lane: string
    /** The lanes the queue offers, for the lane picker where the lanes route lists none. */
    readonly lanes: readonly string[]
    readonly scope: string
    readonly q: string
    readonly filter: ChipFilter
    readonly ticked: readonly TriageCallRow[]
    readonly shown: readonly TriageCallRow[]
  }
): JSX.Element => {
  const [mode, setMode] = useState<Mode>("filter")
  const [predicates, setPredicates] = useState<Predicates>(NO_PREDICATES)
  const selection = selectionOf(mode, { lane, scope, q, filter }, ticked, shown, predicates)

  return (
    <ReplayLauncher selection={selection} lane={lane} lanes={lanes} ticked={ticked.length}>
      <div className="request-row">
        <span className="facet-label">selection</span>
        {MODES.map(([value, label]) => (
          <label key={value}>
            <input type="radio" name="request-mode" checked={mode === value} onChange={() => setMode(value)} />{" "}
            {label}
            {value === "ticked" && ` (${ticked.length})`}
          </label>
        ))}
      </div>

      {mode === "population" && (
        <div className="request-row">
          <label>
            <select
              value={predicates.universe}
              onChange={(event) => setPredicates({ ...predicates, universe: event.target.value === "all" ? "all" : "standing" })}
            >
              <option value="standing">standing</option>
              <option value="all">all</option>
            </select>
          </label>
          <label>
            max-span s{" "}
            <input type="text" className="row-cap" value={predicates.maxSpan} onChange={(e) => setPredicates({ ...predicates, maxSpan: e.target.value })} />
          </label>
          <label>
            green-on{" "}
            <input type="text" value={predicates.greenOn} placeholder="lane" onChange={(e) => setPredicates({ ...predicates, greenOn: e.target.value })} />
          </label>
          <label>
            sample{" "}
            <input type="text" className="row-cap" value={predicates.sample} onChange={(e) => setPredicates({ ...predicates, sample: e.target.value })} />
          </label>
          <label>
            limit{" "}
            <input type="text" className="row-cap" value={predicates.limit} onChange={(e) => setPredicates({ ...predicates, limit: e.target.value })} />
          </label>
        </div>
      )}
    </ReplayLauncher>
  )
}
