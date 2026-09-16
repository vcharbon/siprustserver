/**
 * A leg the platform dialled on a REFER (RFC 3515) is a transferee, not a fork:
 * the join reading moves it off the hunt, the actor stays the callee it is on
 * the wire, and `joined_by` names the REFER that asked for it.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { buildCalls } from "../src/calls.js"
import { synthesize } from "../src/flowsteps.js"
import { policyWith } from "../src/policy.js"
import type { Vantage } from "../src/selection.js"
import { build, type ActorObs, type JoinsReading } from "../src/topology.js"
import type { CallIdDerivation } from "../src/derivation.js"
import {
  CALLEE_CALL_ID,
  CALLEE_URI,
  CALLER_CALL_ID,
  CALLER_URI,
  doc,
  leg,
  oneHop,
  plan,
  request,
  response,
  SOCKETS,
  sutSet
} from "./fixtures.js"

const { caller, sut, callee, other } = SOCKETS

/** The transferee's dialog, derived from the caller's like the callee's is. */
const TRANSFEREE_CALL_ID = `2-${CALLER_CALL_ID}`

/** The platform mints `<n>-<a-leg Call-ID>` for every leg it originates. */
const derivesNumbered: CallIdDerivation = (base, derived) =>
  /^\d+-/.test(derived) && derived.replace(/^\d+-/, "") === base

const ALL_VANTAGES: ReadonlyArray<Vantage> = [
  { leg: 0, hop: 0 },
  { leg: 1, hop: 0 },
  { leg: 2, hop: 0 }
]

/**
 * A answered by B; B REFERs; the platform 202s, dials C, and once C answers
 * releases B. The caller ends the call.
 */
const blindTransferFlows = (): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 9_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 9_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 990, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_010, toTag: "callee-tag" }),
        // The callee asks for the transfer, inside the dialog it answered.
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "REFER", src: callee, dst: sut, ts_ms: 3_000, fromUri: CALLEE_URI, fromTag: "callee-tag", ruri: CALLER_URI, toTag: `from-${CALLEE_CALL_ID}`, headers: ["Refer-To: <sip:+33600000007@10.0.0.1>"] }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 202, reason: "Accepted", cseqMethod: "REFER", src: sut, dst: callee, ts_ms: 3_010, fromUri: CALLEE_URI, fromTag: "callee-tag", toUri: CALLER_URI, toTag: `from-${CALLEE_CALL_ID}` }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 4_100, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 4_105, toTag: "callee-tag" })
      ]),
      leg(TRANSFEREE_CALL_ID, oneHop(sut, other), [
        request({ callId: TRANSFEREE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: other, ts_ms: 3_100 }),
        response({ callId: TRANSFEREE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: other, dst: sut, ts_ms: 4_000, toTag: "other-tag" }),
        request({ callId: TRANSFEREE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: other, ts_ms: 4_010, toTag: "other-tag" }),
        request({ callId: TRANSFEREE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: other, ts_ms: 9_010, toTag: "other-tag" }),
        response({ callId: TRANSFEREE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: other, dst: sut, ts_ms: 9_015, toTag: "other-tag" })
      ])
    ],
    [{ legs: [0, 1, 2] }]
  )

/** The reading a deployment gives: the leg the platform dialled toward `other` joined on a REFER. */
const transfereeJoined = (_flows: Flows.FlowsDoc, actors: ReadonlyArray<ActorObs>): JoinsReading =>
  new Map([[actors.findIndex((a) => a.peer === other), { kind: "refer" as const, evidence: ["stub"] }]])

const cutOf = (joins: typeof transfereeJoined) => {
  const flows = blindTransferFlows()
  const layout = build(flows, ALL_VANTAGES, sutSet(), plan(), derivesNumbered, [], joins)
  const flow = synthesize(flows, layout, plan())
  const calls = buildCalls(flows, layout, flow.t0_us, "A", policyWith({}), flow.steps)
  return { layout, flow, calls }
}

describe("a leg joined on a REFER", () => {
  const { layout, flow, calls } = cutOf(transfereeJoined)
  const transferee = layout.actorsObs.find((a) => a.peer === other)!
  const attempt = calls.calls[0]!.attempts.find((a) => a.leg === transferee.pivotLeg)!

  it("stays a callee, claimed by identity rather than by R-URI position", () => {
    const actor = layout.actors.find((a) => a.id === transferee.actorId)!
    expect(actor.type).toBe("uas")
    expect(actor.claim).toBeUndefined()
  })

  it("is joined by the REFER step, on whichever leg sent it", () => {
    const step = flow.steps.find((s) => s.id === attempt.joined_by!.step)!
    expect(attempt.joined_by!.kind).toBe("refer")
    expect(step.msg.method).toBe("REFER")
    expect(step.leg).toBe("B")
  })

  it("leaves the hunt chain to the callee alone", () => {
    const hunted = calls.calls[0]!.attempts.filter((a) => a.joined_by === undefined)
    expect(hunted.map((a) => [a.branch, a.position])).toEqual([[0, 0]])
    expect(attempt.branch).not.toBe(0)
  })
})

describe("a leg joined as a media resource", () => {
  it("is typed as one", () => {
    const { layout } = cutOf((_flows, actors) =>
      new Map([[actors.findIndex((a) => a.peer === other), { kind: "mrf" as const, evidence: ["stub"] }]])
    )
    const resource = layout.actorsObs.find((a) => a.peer === other)!
    expect(layout.actors.find((a) => a.id === resource.actorId)!.type).toBe("mrf")
  })
})
