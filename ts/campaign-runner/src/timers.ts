/**
 * A cell's lane timers: SUT timers its case names, each read off a lane's
 * `timers` under the lane-file key the pack maps its name to. A run compiles
 * each case once for every lane it runs on, so a lane timer resolves over the
 * run's lanes the case's needs admit, to the first of them stating it (run
 * order); a case none of them states it for is refused naming them. The cell
 * records the value, and a lane stating none or another skips the cell
 * ({@link unrunnableOf}), naming why.
 */
import type { LaneView } from "@sip/rig"
import type { Cell } from "./cell.js"
import * as Results from "@sip/store/campaign-results"

/** The lane-file key (`keepalive_ms`) of each lane timer the pack's cases name, by name. */
export type TimerKeys = Readonly<Record<string, string>>

/** A named timer resolved to its value in ms, or why it does not resolve. */
export type Resolution = { readonly ms: number } | { readonly unresolved: string }

/** The lane-file key of the timer `name`: the one `keys` maps it to, else the name itself. */
const keyOf = (keys: TimerKeys, name: string): string => keys[name] ?? name

/** Whether `lane` holds every capability of `needs`. */
const admits = (lane: LaneView.View, needs: ReadonlyArray<string>): boolean => needs.every((need) => lane.capabilities.has(need))

/** The timer `name` as the first lane of `lanes` admitting `needs` and stating it states it, or why none does. */
export const laneTimer = (keys: TimerKeys, lanes: ReadonlyArray<LaneView.View>) =>
(name: string, needs: ReadonlyArray<string>): Resolution => {
  const key = keyOf(keys, name)
  const admitted = lanes.filter((lane) => admits(lane, needs))
  const stating = admitted.find((lane) => lane.timers[key] !== undefined)
  if (stating !== undefined) return { ms: stating.timers[key] as number }
  const named = admitted.length === 0
    ? "no lane of the run admits the case's needs"
    : `lane ${admitted.map((l) => l.id).join(", ")} states no timers.${key}`
  return { unresolved: named }
}

/** Why `cell` does not run on `lane`, which states a lane timer it was compiled with otherwise, or none; undefined where it states each alike. */
export const unrunnableOf = (keys: TimerKeys, lane: LaneView.View, cell: Cell): string | undefined => {
  for (const [name, ms] of Object.entries(cell.laneTimers ?? {})) {
    const key = keyOf(keys, name)
    const stated = lane.timers[key]
    if (stated === ms) continue
    const states = stated === undefined ? `states no timers.${key}` : `states timers.${key} ${stated}`
    return `${Results.NOT_RUNNABLE}: ${lane.id} ${states}; the cell was compiled with ${name} ${ms} ms (run it on such a lane alone)`
  }
  return undefined
}
