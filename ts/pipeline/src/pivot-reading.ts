/**
 * A pivot read the way a captured call is read: each message step an
 * observation (a `send` is what the system under test received, an `expect`
 * what it emitted), placed by the situation reading (`./situation.ts`), and
 * each leg given its role off the document's calls (`./leg-role.ts`). A
 * launched INVITE pairs with its own call's caller leg.
 *
 * Steps carry no comparable CSeq number (the document renumbers each side), so
 * a response is matched to its request by method alone.
 */
import { Flow } from "@sip/contracts"
import { type CallsView, type LegRole, legRoles } from "./leg-role.js"
import { type Observation, type Placed, place } from "./situation.js"

/** The parts of a pivot this reading uses; the rest is ignored. */
export interface PivotView {
  readonly flow?: ReadonlyArray<Flow.FlowNode>
  readonly calls?: CallsView
}

export interface StepReading {
  /** Step id to its placement; only emitted (`expect`) steps are placed. */
  readonly placed: ReadonlyMap<string, Placed>
  /** Step id to its leg's role. */
  readonly roles: ReadonlyMap<string, LegRole>
}

/**
 * The methods a request may carry outside any dialog. A document flags
 * `in_dialog` on confirmed-dialog requests only, so an unflagged PRACK or
 * UPDATE is an early-dialog request and still carries a To tag.
 */
const OUT_OF_DIALOG = new Set(["INVITE", "OPTIONS", "REGISTER", "MESSAGE", "SUBSCRIBE", "PUBLISH"])

const observationOf = (step: Flow.Step): Observation => {
  const emitted = step.op === "expect"
  const method = Flow.stepMethod(step)
  return step.msg.status === undefined
    ? {
      leg: step.leg,
      emitted,
      request: { method, dialogCreating: step.in_dialog !== true && OUT_OF_DIALOG.has(method) }
    }
    : {
      leg: step.leg,
      emitted,
      response: { status: step.msg.status, method: Flow.stepCseqMethod(step), sdp: carriesSdp(step.msg.body) }
    }
}

const isSdpType = (contentType: string | undefined): boolean =>
  contentType === undefined || /application\/sdp/i.test(contentType)

/** Whether a step's body is a session description, bare, framed in a multipart, or by shape. */
const carriesSdp = (body: Flow.Step["msg"]["body"]): boolean => {
  if (body === undefined) return false
  if ("multipart" in body) return body.multipart.parts.some((p) => isSdpType(p["content-type"]))
  if ("ref" in body) return isSdpType(body["content-type"])
  return body.mode === "sdp-present" || body.mode === "multipart-present"
}

/** Every step's role, and every `expect` step's placement, of one pivot. */
export const readSteps = (pivot: PivotView): StepReading => {
  const steps = (pivot.flow ?? []).flatMap(Flow.flowNodeSteps)
  const roles = legRoles(pivot.calls ?? [])
  const callerOf = new Map<string, string>()
  for (const call of pivot.calls ?? []) for (const a of call.attempts) callerOf.set(a.leg, call.caller_leg)
  const placements = place(steps.map(observationOf), (leg) => callerOf.get(leg))
  const placed = new Map<string, Placed>()
  const stepRoles = new Map<string, LegRole>()
  steps.forEach((step, at) => {
    const p = placements[at]
    if (p !== undefined) placed.set(step.id, p)
    stepRoles.set(step.id, roles.get(step.leg) ?? "other")
  })
  return { placed, roles: stepRoles }
}
