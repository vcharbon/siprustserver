/**
 * The operations a case asks for as steps of its call (upstream ADR-0060
 * appendix §7): a cell asking for an operation its lane binds none of (the
 * lane file's `[operations]`) is not runnable there, forced or not, since
 * nothing on the lane would run its step.
 */
import { Lanes, type LaneView } from "@sip/rig"
import type { Cell } from "./cell.js"
import * as Results from "@sip/store/campaign-results"

/** Why `cell` does not run on the catalog lane `lane`, an operation it asks for unbound there, or `undefined`. */
export const unboundOperationsOf = (lane: LaneView.View, cell: Cell): string | undefined => {
  const unbound = Lanes.unboundOf(lane.loaded, cell.operations ?? [])
  return unbound.length === 0
    ? undefined
    : `${Results.NOT_RUNNABLE}: ${lane.id} binds no operation ${unbound.join(", ")} the case ${cell.ref} asks for`
}
