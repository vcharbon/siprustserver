/**
 * A basic case compiled to its cell (`@sip/rig` `CompiledCell`): one call
 * from the caller's endpoint to the callee's through the system under test,
 * the shape's flow, the session descriptions as resources, and the case's
 * claims beside the document. The document is replayable on every lane, and
 * states no CDR expectation: what a case expects of the system's records is a
 * claim.
 */
import { type Flow, Pivot, Tokens } from "@sip/contracts"
import { ANY_LANE } from "@sip/driver/lanes"
import type { CompiledCell } from "@sip/rig"
import * as Result from "effect/Result"
import { type BasicCase, caseOf, idOf } from "./case.js"
import { RESOURCES } from "./media.js"
import { flowOf } from "./shapes.js"

/**
 * How long the run waits for one expected message, and for the system to
 * settle after the flow, in ms: above the 32 s a SIP transaction may take to
 * time out (64·T1, RFC 3261 §17.1.1.2).
 */
const EXPECT_BUDGET_MS = 37_000
const SETTLE_BUDGET_MS = 37_000

/** The reason token a basic document states for carrying no CDR expectation. */
export const CDR_ABSENT = "claims"

/** The endpoints the parties are bound on, as the document observes them (RFC 5737 addresses; a lane rebinds them). */
const CALLER_ENDPOINT = "192.0.2.10:5060"
const CALLEE_ENDPOINT = "192.0.2.20:5060"

/** One call between two parties, its title naming the cell and its note saying what it plays. */
export interface Call {
  readonly title: string
  readonly caller: string
  readonly callee: string
  readonly note: string
}

/** The pivot document `c` plays. */
export const documentOf = (c: BasicCase): Pivot.PivotV3 =>
  callDocumentOf({ title: c.title, caller: c.caller, callee: c.callee, note: `basic case of shape ${c.shape}` }, flowOf(c).steps)

/**
 * The pivot document playing `steps` between the caller's leg `A` and the
 * callee's leg `B` of `call`, as a basic case's is: a flow a shape does not
 * write keeps the form's parties, endpoints, lanes and budgets.
 */
export const callDocumentOf = (call: Call, steps: ReadonlyArray<Flow.Step>): Pivot.PivotV3 => ({
  pivot_version: Pivot.PIVOT_VERSION,
  case: {
    id: idOf(call.title),
    title: call.title,
    family: "transparent",
    variant: "target",
    origin: "authored",
    lanes: { [ANY_LANE]: Tokens.laneVerdictToken({ _tag: "ok" }) },
    annotations: { notes: [call.note] }
  },
  identities: [
    { name: "caller", kind: "address", observed: call.caller },
    { name: "callee", kind: "address", observed: call.callee }
  ],
  calls: [{ id: "c1", caller_leg: "A", attempts: [{ branch: 0, position: 0, leg: "B", callee: { identity: "callee" } }] }],
  endpoints: [
    { id: "caller", observed: CALLER_ENDPOINT, side: "peer", binding: "dedicated" },
    { id: "callee", observed: CALLEE_ENDPOINT, side: "peer", binding: "dedicated" }
  ],
  actors: [
    { id: "caller", kind: "uac", endpoint: "caller", identity: "caller" },
    { id: "callee", kind: "uas", endpoint: "callee", claim: { by: "arrival-order" } }
  ],
  legs: [{ id: "A", actor: "caller", dir: "out" }, { id: "B", actor: "callee", dir: "in" }],
  flow: [...steps],
  postconditions: { cdr: { absent: CDR_ABSENT } },
  timing: { expect_budget_ms: EXPECT_BUDGET_MS, settle_budget_ms: SETTLE_BUDGET_MS }
})

/** `c` compiled to its cell. */
export const compile = (c: BasicCase): CompiledCell.CompiledCell => ({
  id: idOf(c.title),
  case: c.title,
  title: c.title,
  document: documentOf(c),
  resources: RESOURCES,
  claims: { ...c.claims }
})

/**
 * The cell the basic case `input` states (appendix §14), compiled; a case
 * that is refused throws its `Refused` when the source module defining
 * it loads.
 */
export const defineBasicCase = (input: BasicCase): CompiledCell.CompiledCell => {
  const decoded = caseOf(input)
  if (Result.isFailure(decoded)) throw decoded.failure
  return compile(decoded.success)
}
