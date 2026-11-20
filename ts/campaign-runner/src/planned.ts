/**
 * A cell as the scope running it plans it for the rig's collectors (`@sip/rig`
 * `Context.Planned`): its id, case id and sub-case, its lane, and the claims
 * its collectors read of it beside its document, which the pack states.
 */
import type { Context as RigContext } from "@sip/rig"
import type { Cell } from "./cell.js"
import * as Results from "@sip/store/campaign-results"

/** `cell` as the scope running it on the catalog lane `lane` plans it, with the claims the pack reads of it. */
export const plannedOf = (cell: Cell, lane: string, claims: Readonly<Record<string, unknown>>): RigContext.Planned => ({
  id: cell.id,
  case: Results.caseIdOf(cell.ref, cell.subcase),
  ...(cell.subcase === "" ? {} : { sub_case: cell.subcase }),
  lane,
  claims
})
