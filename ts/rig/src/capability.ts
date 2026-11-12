/**
 * The capabilities the base derives itself (ADR-0060 appendix §11): those of
 * a lane's clock and backend mode, and `operation:<name>` per binding. A part
 * schema's `x-siprig.capabilities` names none of them (`part.ts`): the base
 * ones by {@link derivedByBase}, the bound ones by the capability name rule,
 * which admits no `:`.
 *
 * A run states the capabilities it needs. A lane lacking any of them does not
 * fit it: the run is *not runnable on this lane*; forced, it runs anyway and
 * its results say so ({@link fit}).
 */

/** The capabilities the base derives from a lane's clock and backend mode. */
export const BASE = {
  /** The rig serves the backend the system calls (`served`, or a simulated lane), so recorded decisions replay. */
  scriptedRouting: "scripted-routing",
  /** The rig sits on the system's backend path (`served`, `proxied`, or a simulated lane), so it sees every exchange. */
  routingVisible: "routing-visible",
  /** The system runs on the wall clock. */
  realClock: "real-clock",
  /** The system runs on a paused clock, so a long timer takes no wall-clock time. */
  pausedClock: "paused-clock"
} as const

/** The prefix of the capability a binding in `[operations]` gives: `operation:<name>`. */
export const OPERATION_PREFIX = "operation:"

/** Whether `name` is one the base derives from a lane's clock and backend mode, so no part may give it. */
export const derivedByBase = (name: string): boolean => (Object.values(BASE) as ReadonlyArray<string>).includes(name)

/** What a capture replay needs: the capture's own backend decisions served. */
export const PCAP_REPLAY_NEEDS: ReadonlyArray<string> = [BASE.scriptedRouting]

/** Whether a run fits a lane: it does, it does not, or it was forced onto one it does not fit. */
export type Fit =
  | { readonly _tag: "fits" }
  | { readonly _tag: "refused"; readonly lacks: ReadonlyArray<string>; readonly line: string }
  | { readonly _tag: "forced"; readonly lacks: ReadonlyArray<string>; readonly line: string }

/** How a run needing `needs` (`what`, for the line) fits `lane` holding `has`, `force` stated or not. */
export const fit = (
  lane: string,
  has: ReadonlySet<string>,
  needs: ReadonlyArray<string>,
  what: string,
  force: boolean
): Fit => {
  const lacks = needs.filter((need) => !has.has(need))
  if (lacks.length === 0) return { _tag: "fits" }
  const missing = lacks.join(", ")
  return force
    ? { _tag: "forced", lacks, line: `${what} is forced onto ${lane}, which lacks ${missing}: its results are marked forced` }
    : { _tag: "refused", lacks, line: `${what} is not runnable on this lane: ${lane} lacks ${missing}` }
}
