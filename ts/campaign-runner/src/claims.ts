/**
 * The claims of a cell that nothing on its lane answers (upstream ADR-0060
 * appendix §4), every cell's, whether its document states them or its case
 * source or its compiled case adds them beside it ({@link unansweredOf}):
 * `call-records` is answered on every lane that runs (its judge over a
 * simulated cell's exports, and over the records the `call-records`
 * collector gathers on a hosted lane); any other claim is answered on a
 * lane by the collector of its name the lane wires ({@link requestedOn}), a
 * pack's own on a lane with the part it needs. Where a claim's collector is
 * `required` on the rig path and the lane does not wire it, the lane's batch
 * is refused before any call ({@link requiredUnansweredOf}); otherwise the
 * claim is not requested, and a cell whose every check passed is skipped
 * naming it ({@link notRequestedOf}), never a pass.
 *
 * Every cell's call records are judged, whatever its document claims, so a
 * hosted lane on which the required `call-records` collector is not wired
 * refuses its batch before any call ({@link callRecordsUnwiredOf}).
 */
import { type Pivot, Postcondition } from "@sip/contracts"
import { Collector, CollectorPlan, type LaneView, type RigPath } from "@sip/rig"
import type { Cell } from "./cell.js"
import * as Results from "@sip/store/campaign-results"

/**
 * The collectors of every claim a cell states, nothing on a lane answering
 * them, sorted: the claims `document` states (`Postcondition.statedClaims`)
 * and those `beside` it, but `call-records`, which every lane answers.
 */
export const unansweredOf = (
  document: Pivot.PivotV3,
  beside: Readonly<Record<string, unknown>> = {}
): ReadonlyArray<string> => {
  const stated = [...Object.keys(Postcondition.statedClaims(document.postconditions)), ...Object.keys(beside)]
  return [...new Set(stated)].filter((name) => name !== Postcondition.CALL_RECORDS).sort()
}

/** `cell` on a lane wiring the collectors `wired`, by name: its claims none of them answers. */
export const requestedOn = <C extends Cell>(cell: C, wired: ReadonlySet<string>): C => {
  if (cell.notRequested === undefined) return cell
  const { notRequested, ...rest } = cell
  const left = notRequested.filter((name) => !wired.has(name))
  return (left.length === 0 ? rest : { ...rest, notRequested: left }) as C
}

/**
 * Why `cell`, its every check passed on `lane`, is not a pass, or
 * `undefined`: a claim of a collector no part of the lane answers.
 */
export const notRequestedOf = (lane: string, cell: Cell): string | undefined =>
  cell.notRequested === undefined || cell.notRequested.length === 0
    ? undefined
    : `${Results.NOT_CHECKABLE}: claim(s) not requested: no part of ${lane} answers the collector(s) ${cell.notRequested.join(", ")}`

/**
 * Why the batch of `cells` on `lane` is refused before any call, or
 * `undefined`: a cell claims of a collector `rig` defines as `required`, and
 * nothing on the lane answers it. Names each such collector with the cells
 * claiming it.
 */
export const requiredUnansweredOf = (rig: RigPath.RigPath, lane: string, cells: ReadonlyArray<Cell>): string | undefined => {
  const claimants = new Map<string, Array<string>>()
  for (const cell of cells) {
    for (const name of cell.notRequested ?? []) {
      const defined = rig.collectors.get(name)
      if (defined === undefined || !Collector.isRequired(defined.manifest)) continue
      claimants.set(name, [...(claimants.get(name) ?? []), cell.id])
    }
  }
  if (claimants.size === 0) return undefined
  const named = [...claimants].map(([name, ids]) => `${name} (claimed by ${ids.join(", ")})`)
  return `the required collector(s) ${named.join("; ")} answer nothing on ${lane}: the batch is refused before any call`
}

/**
 * Why a batch on `lane` is refused before any call, or `undefined`: `lane` is
 * hosted and the `call-records` collector `rig` defines as `required` is not
 * wired on it (a part it needs is missing, or the lane excludes it), so no
 * cell's call records would be judged.
 */
export const callRecordsUnwiredOf = (rig: RigPath.RigPath, lane: LaneView.View): string | undefined => {
  const defined = rig.collectors.get(Postcondition.CALL_RECORDS)
  if (lane.kind !== "hosted" || defined === undefined || !Collector.isRequired(defined.manifest)) return undefined
  if (CollectorPlan.isWiredOn(defined, lane.loaded)) return undefined
  const needs = (defined.manifest.needs ?? []).join(", ")
  return `the required collector ${Postcondition.CALL_RECORDS} is not wired on ${lane.id} (it needs the part(s) ${needs}): ` +
    "every cell's call records are judged, so the batch is refused before any call"
}
