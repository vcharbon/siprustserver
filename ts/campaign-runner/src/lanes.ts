/**
 * What each lane of the rig's catalog (`@sip/rig` `lane-catalog.ts`) states
 * for a campaign run: the name a run gives it, why a campaign cannot run on
 * it, its pacing, its timer tolerance and its endpoint rebinding.
 *
 * A run names a lane by its catalog id or, for an id `<prefix><short>` (the
 * pack's prefix), by `<short>`: the short form names its directories under
 * the run, the id its run row. A hosted lane's cell binds every endpoint at
 * port 0 on its SIP target's `endpoint_host`, so the cells of one batch run
 * side by side.
 */
import { Capability, LaneCatalog, type LaneView } from "@sip/rig"
import * as Arr from "effect/Array"
import type { Lane } from "./cell.js"
import type { Pacing } from "./schedule.js"

/** The short form of a lane id: the id without the pack's `prefix`. */
export const shortOf = (prefix: string, id: string): Lane => (id.startsWith(prefix) ? id.slice(prefix.length) : id)

/** The catalog id `name` means: itself where the catalog holds it, else `<prefix><name>`. */
export const resolve = (prefix: string, catalog: LaneCatalog.Catalog, name: string): string =>
  LaneCatalog.find(catalog, name) !== undefined ? name : `${prefix}${name}`

/** The lane `name` (an id or a short form) is, in its short form. */
export const laneNamed = (prefix: string, catalog: LaneCatalog.Catalog, name: string): Lane =>
  shortOf(prefix, resolve(prefix, catalog, name))

/** Every lane of the catalog in its short form, in the catalog's order. */
export const lanesOf = (prefix: string, catalog: LaneCatalog.Catalog): ReadonlyArray<Lane> =>
  catalog.lanes.map((lane) => shortOf(prefix, lane.id))

/** A comma-separated list of lanes (ids or short forms) → the catalog's lanes, or why not. */
export const lanesIn = (
  prefix: string,
  text: string,
  catalog: LaneCatalog.Catalog
): Arr.NonEmptyReadonlyArray<Lane> | string => {
  const known = lanesOf(prefix, catalog)
  const named = text.split(",").map((s) => s.trim()).filter((s) => s !== "").map((s) => laneNamed(prefix, catalog, s))
  const unknown = named.filter((s) => !known.includes(s))
  if (unknown.length > 0) return `unknown lane(s) ${unknown.join(", ")}; lanes: ${known.join(", ")}`
  return Arr.isReadonlyArrayNonEmpty(named) ? named : "no lane named"
}

/** The catalog lane `lane` runs on. */
export const catalogLane = <L extends LaneView.View>(prefix: string, catalog: LaneCatalog.Catalog<L>, lane: Lane): L | undefined =>
  LaneCatalog.find(catalog, resolve(prefix, catalog, lane))

/** Why a campaign cannot run on `found`, or `undefined`: it lacks one of the capabilities `needs` names. */
export const refusalOf = (needs: ReadonlyArray<string>, found: LaneView.View): string | undefined => {
  const fit = Capability.fit(found.id, found.capabilities, needs, "a functional campaign", false)
  return fit._tag === "refused" ? fit.line : undefined
}

/** The campaign pacing and timer tolerance a SIP target takes where it states none. */
export const DEFAULT_CAMPAIGN = { concurrency: 50, starts_per_second: 10, timing_tolerance_ms: 20 } as const

/** The simulated lane's: one process per cell, four at once, its timers 10 ms of virtual time after the backend's answer. */
const SIMULATED_PACING: Pacing = { concurrency: 4 }
const SIMULATED_TOLERANCE_MS = 20

const campaignOf = (found: LaneView.Hosted) => found.sip.campaign ?? DEFAULT_CAMPAIGN

/**
 * How far a timer-anchored dwell may land from the document's value (ms): the
 * SIP target's `campaign.timing_tolerance_ms`, just above the deviation
 * measured on its SUT.
 */
export const timingToleranceOf = (found: LaneView.View): number =>
  found.kind === "simulated" ? SIMULATED_TOLERANCE_MS : campaignOf(found).timing_tolerance_ms

/** The endpoint rebinding of a cell's run-spec on `found`: port 0 on a hosted lane, the document's own on the simulated one. */
export const endpointAddresses = (
  found: LaneView.View,
  endpoints: ReadonlyArray<string>
): Readonly<Record<string, string>> =>
  found.kind === "simulated" ? {} : Object.fromEntries(endpoints.map((id) => [id, `${found.sip.endpoint_host}:0`]))

/** How fast `found` runs its cells: the simulated lane one process per cell, a hosted one at its SIP target's campaign pace. */
export const pacingOf = (found: LaneView.View): Pacing => {
  if (found.kind === "simulated") return SIMULATED_PACING
  const { concurrency, starts_per_second } = campaignOf(found)
  return starts_per_second > 0 ? { concurrency, startIntervalMs: 1000 / starts_per_second } : { concurrency }
}
