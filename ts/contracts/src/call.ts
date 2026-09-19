/**
 * The `calls` block (`PCAP2TEST_PIVOT_V3.md` §4), mirroring `pivot_schema::call`:
 * one entry per call the document plays, each owning its attempt chain and the
 * routing configuration detected for it.
 *
 * Parallel forks are same-`position` attempts on different `branch`es; a
 * sequential hunt is one branch with several positions. A captured case is
 * exactly one call; a concurrency test is several, and their flows interleave
 * through step ids.
 */
import * as Schema from "effect/Schema"
import { Cause, NO_ANSWER_MS_BAND } from "./tokens.js"

/** The dialed target: a name in the registry, never the number itself. */
export const Callee = Schema.Struct({
  identity: Schema.String
})
export interface Callee extends Schema.Schema.Type<typeof Callee> {}

/**
 * The mechanisms that join a leg to a running call. Closed: each names a SIP
 * event the document itself carries as a step — a REFER the platform accepted
 * (RFC 3515), an INFO it accepted as a transfer order (RFC 6086), or the
 * request that inserted a media resource.
 */
export const JoinKind = Schema.Literals(["refer", "info", "mrf"])
export type JoinKind = typeof JoinKind.Type

/** What added a leg to a call that was already running. */
export const JoinedBy = Schema.Struct({
  kind: JoinKind,
  step: Schema.String
})
export interface JoinedBy extends Schema.Schema.Type<typeof JoinedBy> {}

/** An attempt's terminal INVITE final at its own vantage. */
export const Final = Schema.Struct({
  status: Schema.Int,
  at_ms: Schema.Int
})
export interface Final extends Schema.Schema.Type<typeof Final> {}

/** One dialed attempt in a call's chain. */
export const Attempt = Schema.Struct({
  branch: Schema.Int,
  position: Schema.Int,
  leg: Schema.String,
  callee: Callee,
  final: Schema.optionalKey(Final),
  cause: Schema.optionalKey(Cause),
  joined_by: Schema.optionalKey(JoinedBy),
  cause_evidence: Schema.optionalKey(Schema.Array(Schema.String)),
  join_evidence: Schema.optionalKey(Schema.String),
  no_answer_ms: Schema.optionalKey(Schema.Int)
})
export interface Attempt extends Schema.Schema.Type<typeof Attempt> {}

/** Whether `no_answer_ms` is stated exactly where it may be, and inside the armable band. */
export const noAnswerMsIsDeclarable = (attempt: Attempt): boolean => {
  const isNoAnswer = attempt.cause === "no-answer"
  const ms = attempt.no_answer_ms
  if (isNoAnswer && ms !== undefined) return ms >= NO_ANSWER_MS_BAND.min && ms <= NO_ANSWER_MS_BAND.max
  if (isNoAnswer || ms !== undefined) return false
  return true
}

/**
 * The provisional-handling profile the CAPTURED system ran, in the routing API's
 * own vocabulary so a lane applies it without re-deciding. Every token is open.
 */
export const Relay18x = Schema.Struct({
  mode: Schema.String,
  messages: Schema.String,
  prack: Schema.optionalKey(Schema.String),
  evidence: Schema.Array(Schema.String)
})
export interface Relay18x extends Schema.Schema.Type<typeof Relay18x> {}

/**
 * A call the routing decision refused before any dial. The final itself is not
 * restated: `step` names the flow step carrying it, so what the caller got has
 * one home.
 */
export const Refused = Schema.Struct({
  step: Schema.String,
  evidence: Schema.optionalKey(Schema.String)
})
export interface Refused extends Schema.Schema.Type<typeof Refused> {}

/**
 * A call the CALLER abandoned before any dial crossed this vantage. The CANCEL
 * is not restated: `step` names the caller-leg step carrying it, so what the
 * caller sent has one home.
 */
export const Abandoned = Schema.Struct({
  step: Schema.String,
  evidence: Schema.optionalKey(Schema.String)
})
export interface Abandoned extends Schema.Schema.Type<typeof Abandoned> {}

/** One call: who places it, where it is routed, and how provisionals were handled. */
export const Call = Schema.Struct({
  id: Schema.String,
  caller_leg: Schema.String,
  attempts: Schema.Array(Attempt),
  refused: Schema.optionalKey(Refused),
  abandoned: Schema.optionalKey(Abandoned),
  relay18x: Schema.optionalKey(Relay18x)
})
export interface Call extends Schema.Schema.Type<typeof Call> {}
