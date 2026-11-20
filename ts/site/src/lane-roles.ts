/**
 * What each lane runs as the system under test, in a line, and whose log a
 * cell's SUT log is: what a reader of a replay or a campaign result needs to
 * know whose wire and whose logs it is reading, off the site pack's lanes (a
 * hosted lane's SIP target `description`, the pack's line for the simulated
 * lane).
 */
import { LaneCatalog } from "@sip/rig"
import type { SitePack } from "./site-pack.ts"

/** The SUT of `lane` in a line; a lane the pack does not hold says so. */
export const sutOf = (pack: Pick<SitePack.Pack, "lanes" | "simulatedSut">, lane: string): string => {
  const found = LaneCatalog.find(pack.lanes, lane)
  if (found === undefined) return "a lane this deployment does not describe"
  return found.kind === "simulated" ? pack.simulatedSut : found.sip.description
}

/** Every lane of the pack with its SUT. */
export const laneRoles = (pack: Pick<SitePack.Pack, "lanes" | "simulatedSut">): ReadonlyArray<readonly [string, string]> =>
  pack.lanes.lanes.map((lane) => [lane.id, sutOf(pack, lane.id)] as const)

/**
 * Whose log a cell's SUT log attachment titled `title` is: always the SUT's
 * on the cell's lane, the container the lane's SUT log part names.
 */
export const sutLogRole = (title: string): string => `${title}, the SUT on this lane`
