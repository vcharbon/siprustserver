/**
 * The pivot v3 document (`PCAP2TEST_PIVOT_V3.md` §2), mirroring
 * `pivot_schema::document`: the top level and the budgets stated once.
 *
 * Design rule, and the one that decides every field: **smart compiler, dumb
 * interpreter.** A corner case is compiled into explicit fields by the
 * generator, never inferred at replay time. There is no control flow — the
 * declared alternatives of an `alt` are the only branching — and every object
 * refuses unknown fields, so a misspelling fails loudly rather than silently
 * changing a replay's meaning.
 *
 * **Emptiness is uniform**: every optional collection is OMITTED when empty.
 * Never `[]`, never `{}`, never `null`. The six required top-level vectors are
 * the exception: they appear even empty, because they are required.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { Call } from "./call.js"
import { format } from "./canonical.js"
import { Case } from "./case.js"
import { Deviation } from "./deviation.js"
import { FlowNode, flowNodeSteps, type Step } from "./flow.js"
import { Identity } from "./identity.js"
import { MustFail } from "./must-fail.js"
import { Actor, Endpoint, Leg } from "./placement.js"
import { Postconditions } from "./postcondition.js"
import { STRICT } from "./strict.js"
import { RfcViolation } from "./violation.js"

/** The `pivot_version` this contract models. */
export const PIVOT_VERSION = 3

/** Budgets stated once for the whole case. */
export const Timing = Schema.Struct({
  expect_budget_ms: Schema.Int,
  settle_budget_ms: Schema.Int,
  capture_span_ms: Schema.optionalKey(Schema.Int)
})
export interface Timing extends Schema.Schema.Type<typeof Timing> {}

/** One replayable case: a captured call reproduced, or an authored test. */
export const PivotV3 = Schema.Struct({
  pivot_version: Schema.Int,
  case: Case,
  identities: Schema.Array(Identity),
  calls: Schema.Array(Call),
  endpoints: Schema.Array(Endpoint),
  actors: Schema.Array(Actor),
  legs: Schema.Array(Leg),
  flow: Schema.Array(FlowNode),
  deviations: Schema.optionalKey(Schema.Array(Deviation)),
  rfc_violations: Schema.optionalKey(Schema.Array(RfcViolation)),
  must_fail: Schema.optionalKey(Schema.Array(MustFail)),
  postconditions: Schema.optionalKey(Postconditions),
  /** RESERVED (§12): the deployment-extensible media vocabulary. Nothing reads it yet. */
  media: Schema.optionalKey(Schema.Record(Schema.String, Schema.Unknown)),
  timing: Timing
})
export interface PivotV3 extends Schema.Schema.Type<typeof PivotV3> {}

export const decodePivot = Schema.decodeUnknownEffect(PivotV3, STRICT)
export const decodePivotSync = Schema.decodeUnknownSync(PivotV3, STRICT)
export const encodePivot = Schema.encodeUnknownSync(PivotV3, STRICT)

/** Parse a pivot document from its text. */
export const parsePivot = (text: string) => Effect.suspend(() => decodePivot(JSON.parse(text) as unknown))

/** Serialize canonically (§2.1): keys sorted, two-space indent, one trailing newline. */
export const emitPivot = (pivot: PivotV3): string => format(encodePivot(pivot))

/** Whether the document declares the version this contract models. */
export const versionMatches = (pivot: PivotV3): boolean => pivot.pivot_version === PIVOT_VERSION

/**
 * Every message step in the document, `alt` branches and `unordered` groups
 * included, in document order.
 */
export const pivotSteps = (pivot: PivotV3): ReadonlyArray<Step> => pivot.flow.flatMap(flowNodeSteps)
