/**
 * The operations a case asks for as steps of its call (upstream ADR-0060
 * appendix §7, `@sip/rig` `OperationStep`): a case asking for one carries a
 * tag stating alone, or it is refused at compile, since other cells on its
 * lane would run through the operation; a cell asking for an operation its
 * lane binds none of (the lane file's `[operations]`) is not runnable there,
 * forced or not, since nothing on the lane would run its step.
 */
import { Lanes, type LaneView } from "@sip/rig"
import type { Cell } from "./cell.js"
import * as Results from "@sip/store/campaign-results"

/** Why the cell `id`, asking for `operations` as steps and running `alone` or not, does not compile, or `undefined`. */
export const aloneRefusalOf = (id: string, operations: ReadonlyArray<string>, alone: boolean): string | undefined =>
  operations.length === 0 || alone
    ? undefined
    : `${id}: injects ${operations.join(", ")} as a step of its call, and carries no tag stating alone: ` +
      "other cells on its lane would run through it"

/** Why `cell` does not run on the catalog lane `lane`, an operation it asks for unbound there, or `undefined`. */
export const unboundOperationsOf = (lane: LaneView.View, cell: Cell): string | undefined => {
  const unbound = Lanes.unboundOf(lane.loaded, cell.operations ?? [])
  return unbound.length === 0
    ? undefined
    : `${Results.NOT_RUNNABLE}: ${lane.id} binds no operation ${unbound.join(", ")} the case ${cell.ref} asks for`
}
