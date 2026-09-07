/**
 * The `flow` (`PCAP2TEST_PIVOT_V3.md` §6), mirroring `pivot_schema::flow`: the
 * choreography, in order.
 *
 * Four node kinds, discriminated by `op`: a message step (`send` / `expect`), an
 * injected external event (`inject`), a set of declared alternatives (`alt`) and
 * an order-free group (`unordered`). Nesting stops there — an `alt` branch and
 * an `unordered` group hold message steps, never further blocks.
 *
 * Same-leg ordering is list order; cross-leg ordering is `after`, which names the
 * steps that must have completed first. Ordering is message-mediated: there are
 * no state predicates.
 */
import * as Schema from "effect/Schema"
import { Check } from "./check.js"
import { MsgSpec } from "./msg.js"
import { Anchor } from "./tokens.js"

/** Whether the actor emits the message or waits for it. */
export const Op = Schema.Literals(["send", "expect"])
export type Op = typeof Op.Type

/**
 * What an `expect` does with the content it stored. The GENERATOR decides and
 * the document says which; a lane never picks the meaning.
 */
export const CheckMode = Schema.Literals(["assert", "record"])
export type CheckMode = typeof CheckMode.Type

/** Dwell from an explicit anchor. There are no absolute times in a pivot. */
export const Delay = Schema.Struct({
  ms: Schema.Int,
  from: Anchor,
  compressible: Schema.Boolean,
  timer_linked: Schema.Boolean
})
export interface Delay extends Schema.Schema.Type<typeof Delay> {}

/**
 * The step's coordinate in the flows document. Informative: the interpreter
 * never reads it, the post-run confrontation pairs a run step with the captured
 * message it is compared against.
 */
export const Observed = Schema.Struct({
  leg: Schema.Int,
  msg: Schema.Int,
  at_us: Schema.Int
})
export interface Observed extends Schema.Schema.Type<typeof Observed> {}

/**
 * One message at one actor's vantage. `auto`, `in_dialog`, `confirms_dialog` and
 * `optional` are omitted when false: no field's emptiness carries meaning (§2.2).
 */
export const Step = Schema.Struct({
  id: Schema.String,
  leg: Schema.String,
  op: Op,
  auto: Schema.optionalKey(Schema.Boolean),
  in_dialog: Schema.optionalKey(Schema.Boolean),
  confirms_dialog: Schema.optionalKey(Schema.Boolean),
  retransmits: Schema.optionalKey(Schema.Int),
  /**
   * The gap, in ms, before each repeat this step declares — one entry per
   * repeat, measured from the emission before it. A CAPTURED ladder's own
   * pacing: `retransmits` says how many rungs and this says when, so a peer
   * whose ladder is not the RFC's is replayed as it ran. Absent — on every
   * authored step, and on a generated one whose producer measured none — the
   * ladder falls back to the RFC pacing for the message class (§6.9).
   */
  retransmit_intervals_ms: Schema.optionalKey(Schema.Array(Schema.Int)),
  check: Schema.optionalKey(CheckMode),
  optional: Schema.optionalKey(Schema.Boolean),
  after: Schema.optionalKey(Schema.Array(Schema.String)),
  checks: Schema.optionalKey(Schema.Array(Check)),
  early: Schema.optionalKey(Schema.String),
  overlap: Schema.optionalKey(Schema.String),
  msg: MsgSpec,
  delay: Delay,
  within_ms: Schema.optionalKey(Schema.Int),
  observed: Schema.optionalKey(Observed)
})
export interface Step extends Schema.Schema.Type<typeof Step> {}

/**
 * An external event the lane's injector performs. The interpreter NEVER executes
 * an action: it hands the token to an injector interface the deployment
 * provides, which is why one op spans store faults, HTTP-fabric faults and node
 * kills.
 */
export const Inject = Schema.Struct({
  id: Schema.String,
  op: Schema.Literal("inject"),
  action: Schema.String,
  target: Schema.optionalKey(Schema.String),
  after: Schema.optionalKey(Schema.Array(Schema.String)),
  delay: Schema.optionalKey(Delay)
})
export interface Inject extends Schema.Schema.Type<typeof Inject> {}

/** One alternative. Its `name` is what `${step:<alt-id>.branch}` resolves to. */
export const Branch = Schema.Struct({
  name: Schema.String,
  steps: Schema.Array(Step)
})
export interface Branch extends Schema.Schema.Type<typeof Branch> {}

/**
 * Declared alternatives. The interpreter commits on the first discriminating
 * message and NEVER backtracks, so branches must differ at their first message.
 */
export const Alt = Schema.Struct({
  id: Schema.String,
  op: Schema.Literal("alt"),
  branches: Schema.Array(Branch),
  after: Schema.optionalKey(Schema.Array(Schema.String))
})
export interface Alt extends Schema.Schema.Type<typeof Alt> {}

/** Messages that must ALL arrive, in any order. They share one place in the ordering. */
export const Unordered = Schema.Struct({
  id: Schema.String,
  op: Schema.Literal("unordered"),
  steps: Schema.Array(Step),
  after: Schema.optionalKey(Schema.Array(Schema.String))
})
export interface Unordered extends Schema.Schema.Type<typeof Unordered> {}

/** One node of the flow, discriminated by `op`. */
export const FlowNode = Schema.Union([Step, Inject, Alt, Unordered])
export type FlowNode = typeof FlowNode.Type

export const isStepNode = (node: FlowNode): node is Step => node.op === "send" || node.op === "expect"
export const isInjectNode = (node: FlowNode): node is Inject => node.op === "inject"
export const isAltNode = (node: FlowNode): node is Alt => node.op === "alt"
export const isUnorderedNode = (node: FlowNode): node is Unordered => node.op === "unordered"

/** The node's own id. */
export const flowNodeId = (node: FlowNode): string => node.id

/** The steps this node orders after. */
export const flowNodeAfter = (node: FlowNode): ReadonlyArray<string> => node.after ?? []

/**
 * Every message step the node contains, in document order. The one traversal
 * lint, formatters and interpreters share, so a construct added to the union
 * cannot be missed by one of them.
 */
export const flowNodeSteps = (node: FlowNode): ReadonlyArray<Step> => {
  if (isStepNode(node)) return [node]
  if (isAltNode(node)) return node.branches.flatMap((branch) => branch.steps)
  if (isUnorderedNode(node)) return node.steps
  return []
}

/**
 * The node with every message step it contains replaced by `f(step)`, in place
 * and in order; a node holding none is returned as it is. The rewriting twin of
 * {@link flowNodeSteps}, so a construct added to the union is missed by neither.
 */
export const mapFlowNodeSteps = (node: FlowNode, f: (step: Step) => Step): FlowNode => {
  if (isStepNode(node)) return f(node)
  if (isAltNode(node)) {
    return { ...node, branches: node.branches.map((branch) => ({ ...branch, steps: branch.steps.map(f) })) }
  }
  if (isUnorderedNode(node)) return { ...node, steps: node.steps.map(f) }
  return node
}

// --- SIP facts read off one document step ---------------------------------------------
//
// The same facts `./flows.ts` reads off a captured message, read off a step's
// `msg` spec, under the same names. A step states `method` on a request and
// `status` + `cseq-method` on a response; absent means the document is silent.

/** The request method, uppercased; empty on a response step. */
export const stepMethod = (step: Step): string => (step.msg.method ?? "").toUpperCase()

/** The transaction's method, uppercased; empty where the step states none. */
export const stepCseqMethod = (step: Step): string => (step.msg["cseq-method"] ?? "").toUpperCase()

/** A request step of `method`. */
export const isRequest = (step: Step, method: string): boolean =>
  step.msg.status === undefined && stepMethod(step) === method.toUpperCase()

/** A response step whose transaction is `method`. */
export const isResponseTo = (step: Step, method: string): boolean =>
  step.msg.status !== undefined && stepCseqMethod(step) === method.toUpperCase()

/** The status of a response to an INVITE; `undefined` for anything else. */
export const inviteStatus = (step: Step): number | undefined =>
  isResponseTo(step, "INVITE") ? step.msg.status : undefined

/** A provisional a B2BUA passes on rather than mints: `180`–`189` to an INVITE. */
export const isProvisionalToInvite = (step: Step): boolean => {
  const status = inviteStatus(step)
  return status !== undefined && status >= 180 && status < 190
}

/** A final answering an INVITE: the response that ENDS its transaction. */
export const isFinalToInvite = (step: Step): boolean => (inviteStatus(step) ?? 0) >= 200

/** A 2xx answering an INVITE: the response that ESTABLISHES a dialog. */
export const isSuccessToInvite = (step: Step): boolean => {
  const status = inviteStatus(step)
  return status !== undefined && status >= 200 && status < 300
}

/**
 * A dialog-creating INVITE: the initial offer, not a re-INVITE. The document
 * states it as `in_dialog`; the capture states it as an empty To-tag
 * (`Flows.opensDialog`), and the two are the same claim.
 */
export const opensDialog = (step: Step): boolean => isRequest(step, "INVITE") && step.in_dialog !== true
