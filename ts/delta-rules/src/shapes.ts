/**
 * Shape probes read: strays, extra and missing messages, status substitutions
 * and the status sets a family row states.
 */
import type { Probe } from "@sip/pipeline"
import { statusOf } from "./scope.js"

type ShapeProbe = Probe.ShapeProbe

/** A stray of `method`: a request the flow never scripted, serviced or extra. */
export const stray = (s: ShapeProbe, method: string): boolean =>
  (s.shapeKind.shape === "serviced-stray" || s.shapeKind.shape === "extra-message") && s.shapeKind.method === method

/** A stray of `method` the scripted endpoint answered on its own. */
export const autoAnswered = (s: ShapeProbe, method: string): boolean =>
  s.shapeKind.shape === "serviced-stray" && s.shapeKind.method === method &&
  s.shapeKind.action === "auto-reacted"

/**
 * A message of `method`, or of a response as the interpreter names it
 * (`200-to-CANCEL`), one side carried and the other not; compared exactly.
 */
export const message = (s: ShapeProbe, shape: "extra-message" | "missing-message", method: string): boolean =>
  s.shapeKind.shape === shape && s.shapeKind.method === method

/** The status substitution a shape probe is, or `undefined`. */
export const statusSubstitution = (s: ShapeProbe) =>
  s.shapeKind.shape === "status-substitution" ? s.shapeKind : undefined

/** A set of statuses: one, a list, or a class (`"2xx"`). */
export type Statuses = number | ReadonlyArray<number> | `${1 | 2 | 3 | 4 | 5 | 6}xx`

/** Whether `status` is in `set`. */
export const inStatuses = (status: number | undefined, set: Statuses): boolean => {
  if (status === undefined) return false
  if (typeof set === "number") return status === set
  if (typeof set === "string") return Math.floor(status / 100) === Number(set[0])
  return set.includes(status)
}

/** A status substitution as a family row states it. */
export interface Substitution {
  /** The status the capture's side was answered; any, none included, where absent. */
  readonly captured?: Statuses
  /** The status the replay's side was answered. */
  readonly observed: Statuses
  /** The response's CSeq method; any where absent. */
  readonly cseq?: string
  /** The initial INVITE's response, or any other; either where absent. */
  readonly at?: "initial-invite" | "response"
}

/** Whether `s` is the status substitution `row` states. */
export const substituted = (s: ShapeProbe, row: Substitution): boolean => {
  const sub = statusSubstitution(s)
  if (sub === undefined || !inStatuses(sub.observed, row.observed)) return false
  if (row.captured !== undefined && !inStatuses(sub.captured, row.captured)) return false
  if (row.at !== undefined && s.scope?.kind !== row.at) return false
  return row.cseq === undefined || statusOf(s.scope, row.cseq) !== undefined
}

/** The class bucket a final falls in: 2xx, 3xx, or 4xx and 5xx as one; `undefined` outside them. */
export const finalClassBucket = (status: number): number | undefined => {
  if (status >= 200 && status <= 299) return 2
  if (status >= 300 && status <= 399) return 3
  if (status >= 400 && status <= 599) return 4
  return undefined
}

/** How long an INVITE server transaction may answer in before it owes a 100 (RFC 3261 §17.2.1). */
export const TRYING_DEADLINE_MS = 200

/** A shape probe's step inside `steps`, or, unattributed (`""`), any step there at all. */
export const atOrUnattributed = (s: ShapeProbe, steps: ReadonlySet<string>): boolean =>
  s.step === "" ? steps.size > 0 : steps.has(s.step)
