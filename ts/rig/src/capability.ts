/**
 * The capabilities the base derives itself (ADR-0060 appendix §11): those of
 * a lane's clock and backend mode, and `operation:<name>` per binding. A part
 * schema's `x-siprig.capabilities` names none of them (`part.ts`).
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

/** Whether the base derives `name` itself, so no part may give it. */
export const derivedByBase = (name: string): boolean =>
  (Object.values(BASE) as ReadonlyArray<string>).includes(name) || name.startsWith(OPERATION_PREFIX)
