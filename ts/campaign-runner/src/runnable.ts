/**
 * Which lanes of the rig's catalog a campaign may run on here, and which run
 * together, over the pack's refusal of a lane ({@link Refusal}). The lanes of
 * one run are of one lane group (`@sip/rig` `LaneCatalog.groupOf`: the part of
 * each shared kind they name): a pick of two groups is refused, and the
 * remedy names the groups the runnable lanes form. A run that names no lane
 * takes the first runnable lane's group.
 */
import { LaneCatalog, type LaneView } from "@sip/rig"
import * as Result from "effect/Result"

/** Why a campaign does not run on a lane here, as the pack states it; null where it does. */
export type Refusal<L extends LaneView.View> = (lane: L) => string | null

/** One catalog lane as a campaign start reads it. */
export interface Standing {
  readonly id: string
  /** The lanes of one group run together. */
  readonly group: string
  /** Why a campaign does not run on it here; null where it does. */
  readonly refused: string | null
}

/** Every lane of `catalog`, in its order, with its group and its refusal. */
export const standingsOf = <L extends LaneView.View>(refusal: Refusal<L>, catalog: LaneCatalog.Catalog<L>): ReadonlyArray<Standing> =>
  catalog.lanes.map((lane) => ({ id: lane.id, group: LaneCatalog.groupOf(catalog, lane), refused: refusal(lane) }))

/** What runs instead: the groups of the lanes `catalog` runs here, as a line. */
export const remedyOf = <L extends LaneView.View>(refusal: Refusal<L>, catalog: LaneCatalog.Catalog<L>): string => {
  const runnable = catalog.lanes.filter((lane) => refusal(lane) === null)
  if (runnable.length === 0) return "no lane of this environment runs a campaign"
  return `run ${LaneCatalog.groupsLine(LaneCatalog.groupsOf(catalog, runnable).map((group) => group.map((lane) => lane.id)))}`
}

/** The lanes a run that names none takes: every runnable lane of the first runnable lane's group, in the catalog's order; none where no lane runs here. */
export const defaultPick = <L extends LaneView.View>(refusal: Refusal<L>, catalog: LaneCatalog.Catalog<L>): ReadonlyArray<L> =>
  LaneCatalog.groupsOf(catalog, catalog.lanes.filter((lane) => refusal(lane) === null))[0] ?? []

/** Why `picked` of `catalog` cannot run as one campaign, or null: a lane refused here, or two groups. */
export const pickRefusal = <L extends LaneView.View>(
  refusal: Refusal<L>,
  catalog: LaneCatalog.Catalog<L>,
  picked: ReadonlyArray<L>
): string | null => {
  for (const lane of picked) {
    const refused = refusal(lane)
    if (refused !== null) return refused
  }
  const shared = LaneCatalog.shared(catalog, picked)
  return Result.isFailure(shared) ? shared.failure : null
}

/** The files the lanes of `picked` were read from, one line per lane: its own, then every part it names, in its file's order. */
export const filesOf = (picked: ReadonlyArray<LaneView.View>): ReadonlyArray<string> =>
  picked.map((lane) => {
    const own = lane.file ?? "built in, no file"
    const parts = [...lane.loaded.parts.values()].map((part) => `${part.kind} ${part.name}: ${part.file}`)
    return `${lane.id}: ${own}${parts.length === 0 ? "" : `; ${parts.join("; ")}`}`
  })

/** `refused`, the lane files `picked` was read from, and what runs instead. */
export const explained = <L extends LaneView.View>(
  refusal: Refusal<L>,
  refused: string,
  picked: ReadonlyArray<L>,
  catalog: LaneCatalog.Catalog<L>
): string =>
  [
    refused,
    `  the lanes read (${catalog.dirs.lanes}):`,
    ...filesOf(picked).map((line) => `    ${line}`),
    `  instead: ${remedyOf(refusal, catalog)}`
  ].join("\n")
