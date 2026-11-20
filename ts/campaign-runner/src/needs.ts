/**
 * A case's own capability needs on a lane. Every campaign case needs what the
 * campaign harness does (a lane refusal of the whole run); a case states what
 * it needs beyond it (`needs`), among the capabilities the pack names. On a
 * lane lacking one the case is not runnable, a skip; forced, it runs, and the
 * lane's run row records what the lane lacked.
 */
import { Capability, type LaneView } from "@sip/rig"
import type { CaseFile } from "./case-file.js"
import type { Cell } from "./cell.js"
import * as Results from "@sip/store/campaign-results"

/** The capabilities a case file's default export says its case needs among `known`; a name of none is the compiler's to refuse. */
const needsOf = (known: ReadonlyArray<string>, value: unknown): ReadonlyArray<string> => {
  const needs = typeof value === "object" && value !== null ? (value as { needs?: unknown }).needs : undefined
  return Array.isArray(needs) ? needs.filter((need): need is string => known.includes(need)) : []
}

/** Why `cell` does not run on the catalog lane `lane`, lacking a capability its case needs and unforced, or `undefined`. */
export const unfitOf = (lane: LaneView.View, cell: Cell, force: boolean): string | undefined => {
  const fit = Capability.fit(lane.id, lane.capabilities, cell.needs, `the case ${cell.ref}`, force)
  return fit._tag === "refused" ? `${Results.NOT_RUNNABLE}: ${fit.line}` : undefined
}

/**
 * What the catalog lane `lane` lacks of the needs among `known` of the cases
 * `files` state (those of `ref` alone where it is named, and those `runs`
 * keeps on the lane), sorted: what its run row records a forced run lacked.
 */
export const forcedOf = (
  known: ReadonlyArray<string>,
  lane: LaneView.View,
  files: ReadonlyArray<CaseFile>,
  ref: string | undefined,
  runs: (file: CaseFile) => boolean = () => true
): ReadonlyArray<string> => {
  const lacks = new Set<string>()
  for (const file of files) {
    if (ref !== undefined && (file.value as { ref?: unknown } | null)?.ref !== ref) continue
    if (!runs(file)) continue
    for (const need of needsOf(known, file.value)) if (!lane.capabilities.has(need)) lacks.add(need)
  }
  return [...lacks].sort()
}
