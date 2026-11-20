/**
 * The minimal case form (ADR-0060 appendix §14): two parties as plain SIP
 * addresses, the shape their call takes and its parameters, the checks on the
 * final the caller receives, the claims the case makes of collectors, the
 * campaign tags it carries, and the operations injected as steps of its call.
 *
 * A case is refused when it does not decode (a key the form does not name, an
 * address that is no `sip:`/`sips:` URI with a user part, a status the shape
 * does not play, a tag or an operation that is no name), when its title
 * yields no cell id, when a `status` check of its final expects another
 * status than the one its shape ends with, and when an operation step is at a
 * point its shape does not reach or shares its point with another.
 */
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"

/** A plain SIP address: a `sip:` or `sips:` URI naming a user at a host (RFC 3261 §19.1), no parameter. */
export const Address = Schema.String.check(
  Schema.makeFilter((address) => /^sips?:[^@\s<>;?:]+@[^@\s<>;?]+$/.test(address) || "a sip: or sips: URI naming a user at a host")
)

/** A non-empty string. */
const Text = Schema.String.check(Schema.isNonEmpty())

/** One check on the final the caller receives: the field (as the pivot check vocabulary names it) equals the value. */
export const FinalCheck = Schema.Struct({
  field: Text,
  eq: Schema.Union([Schema.String, Schema.Int])
})
export interface FinalCheck extends Schema.Schema.Type<typeof FinalCheck> {}

/** A collector, operation or campaign tag name: lower-case letters, digits and dashes. */
const Name = Schema.String.check(Schema.isPattern(/^[a-z0-9][a-z0-9-]*$/, { title: "a name of lower-case letters, digits and dashes" }))

/**
 * A point of the call an operation step injects at: `reached`, once the
 * INVITE reached the callee; `answered`, once the callee received the ACK of
 * its 200, the call up.
 */
export const Point = Schema.Literals(["reached", "answered"])
export type Point = typeof Point.Type

/**
 * An operation as a step of the call (ADR-0060 appendix §7): the operation
 * the lane binds by this name, injected at `at`; the call's next message sent
 * waits for its inject to answer. `forgets`, at `answered` only: the system
 * loses the call to the operation (a restart), so each party ends its leg
 * with a BYE the system answers 481 (RFC 3261 §15.1.2).
 */
export const OperationStep = Schema.Struct({
  operation: Name,
  at: Point,
  forgets: Schema.optionalKey(Schema.Boolean)
})
export interface OperationStep extends Schema.Schema.Type<typeof OperationStep> {}

const Common = {
  title: Text,
  /** The party that calls. */
  caller: Address,
  /** The party called: the INVITE's Request-URI and To. */
  callee: Address,
  final: Schema.Array(FinalCheck),
  /** What the case expects of each collector, by its name (ADR-0060 appendix §4). */
  claims: Schema.optionalKey(Schema.Record(Name, Schema.Unknown)),
  /** The campaign tags the case carries; a case stating a step carries one stating alone, or its campaign refuses it. */
  tags: Schema.optionalKey(Schema.Array(Name)),
  /** The operations injected as steps of the call, one per point. */
  steps: Schema.optionalKey(Schema.Array(OperationStep))
}

/** Which party hangs up the answered call. */
export const Ender = Schema.Literals(["caller", "callee"])
export type Ender = typeof Ender.Type

/** The callee answers; the call is held, then `ends` (the caller by default) hangs up. */
export const BasicCall = Schema.Struct({
  ...Common,
  shape: Schema.Literal("basic_call"),
  params: Schema.optionalKey(Schema.Struct({ ends: Schema.optionalKey(Ender) }))
})
export interface BasicCall extends Schema.Schema.Type<typeof BasicCall> {}

/**
 * The finals a callee rejects an INVITE with that need no header beside the
 * status line (RFC 3261 §21) and that reach the caller as sent, with their
 * reason phrases. A 401 or 407 needs its challenge, a 405 its Allow, a 420
 * its Unsupported; 481, 487 and 491 answer states this shape never reaches;
 * and a proxy relays a 503 as a 500 (§16.7 step 6).
 */
export const REJECTS = {
  403: "Forbidden",
  404: "Not Found",
  408: "Request Timeout",
  410: "Gone",
  480: "Temporarily Unavailable",
  484: "Address Incomplete",
  486: "Busy Here",
  488: "Not Acceptable Here",
  500: "Server Internal Error",
  600: "Busy Everywhere",
  603: "Decline",
  604: "Does Not Exist Anywhere",
  606: "Not Acceptable"
} as const

/** A status {@link REJECTS} names. */
export const RejectStatus = Schema.Literals([403, 404, 408, 410, 480, 484, 486, 488, 500, 600, 603, 604, 606])
export type RejectStatus = typeof RejectStatus.Type

/** The status an `invite_reject` plays where its parameters state none. */
export const DEFAULT_REJECT: RejectStatus = 486

/** The callee rejects the INVITE with `status` (486 Busy Here by default). */
export const InviteReject = Schema.Struct({
  ...Common,
  shape: Schema.Literal("invite_reject"),
  params: Schema.optionalKey(Schema.Struct({ status: Schema.optionalKey(RejectStatus) }))
})
export interface InviteReject extends Schema.Schema.Type<typeof InviteReject> {}

export const BasicCase = Schema.Union([BasicCall, InviteReject])
export type BasicCase = typeof BasicCase.Type

/** Why a case is refused. */
export class Refused extends Schema.TaggedError<Refused>()("BasicCase.Refused", {
  title: Schema.String,
  reason: Schema.String
}) {
  override get message(): string {
    return `basic case ${JSON.stringify(this.title)}: ${this.reason}`
  }
}

/** The cell id a title names: its lower-cased words joined by dashes. */
export const idOf = (title: string): string =>
  title.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "")

/** The final status the case's shape ends the INVITE with. */
export const finalStatusOf = (c: BasicCase): number =>
  c.shape === "basic_call" ? 200 : (c.params?.status ?? DEFAULT_REJECT)

/** The points each shape's call passes, in its order. */
export const POINTS: Readonly<Record<BasicCase["shape"], ReadonlyArray<Point>>> = {
  basic_call: ["reached", "answered"],
  invite_reject: ["reached"]
}

/** Why `c`'s operation steps do not hold, or `undefined`: a point its shape does not reach, two steps at one point. */
const stepsMeaningOf = (c: BasicCase): string | undefined => {
  const points = POINTS[c.shape]
  const steps = c.steps ?? []
  const astray = steps.find((step) => !points.includes(step.at))
  if (astray !== undefined) {
    return `steps: ${astray.operation} is at ${astray.at}, which ${c.shape} does not reach (its points: ${points.join(", ")})`
  }
  const early = steps.find((step) => step.forgets === true && step.at !== "answered")
  if (early !== undefined) return `steps: ${early.operation} forgets the call at ${early.at}: a call is forgotten once answered`
  for (const point of points) {
    const at = steps.filter((step) => step.at === point).map((step) => step.operation).sort()
    if (at.length > 1) return `steps: ${at.join(" and ")} are both at ${point}: one step per point`
  }
  return undefined
}

/** Why a decoded case does not hold, or `undefined`. */
const meaningOf = (c: BasicCase): string | undefined => {
  if (idOf(c.title) === "") return "its title holds no letter or digit to name its cell by"
  const status = finalStatusOf(c)
  const contrary = c.final.find((check) => check.field === "status" && String(check.eq) !== String(status))
  return contrary === undefined
    ? stepsMeaningOf(c)
    : `final: expects status ${contrary.eq}, and ${c.shape} ends the INVITE with ${status}`
}

const decode = Schema.decodeUnknownResult(BasicCase, { onExcessProperty: "error", errors: "first" })

/** `input` as a case, or why it is refused. */
export const caseOf = (input: unknown): Result.Result<BasicCase, Refused> => {
  const title = typeof input === "object" && input !== null && "title" in input ? String(input.title) : ""
  const decoded = decode(input)
  if (Result.isFailure(decoded)) return Result.fail(new Refused({ title, reason: decoded.failure.message.split("\n").join(" ") }))
  const why = meaningOf(decoded.success)
  return why === undefined ? Result.succeed(decoded.success) : Result.fail(new Refused({ title, reason: why }))
}
