/**
 * A caller-facing provisional beyond the peer emissions that anchor it is a
 * tolerated absence: the platform's spare copy, not a datagram a relay is
 * caused to send (§6.9, issue 106).
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { synthesize } from "../src/flowsteps.js"
import { build } from "../src/topology.js"
import type { Vantage } from "../src/selection.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  derivesOnePrefix,
  doc,
  leg,
  oneHop,
  plan,
  request,
  response,
  SOCKETS,
  sutSet
} from "./fixtures.js"

const BOTH_VANTAGES: ReadonlyArray<Vantage> = [
  { leg: 0, hop: 0 },
  { leg: 1, hop: 0 }
]

const flowOf = (flows: Flows.FlowsDoc) =>
  synthesize(flows, build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix), plan())

const { caller, callee, sut } = SOCKETS

/** One caller-facing 180, optionally carrying the identity header a spare drops. */
const callerRing = (ts_ms: number, headers?: ReadonlyArray<string>, retx = false): Flows.Msg => ({
  ...response({
    callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE",
    src: sut, dst: caller, ts_ms, toTag: "sut-tag", headers
  }),
  retx,
  ...(retx ? { repeat_of: 2 } : {})
})

const calleeRing = (ts_ms: number): Flows.Msg =>
  response({
    callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE",
    src: callee, dst: sut, ts_ms, toTag: "callee-tag"
  })

/**
 * An answered call whose caller leg carries `callerRings` provisionals against
 * the callee leg's `calleeRings`.
 */
const ringingCall = (
  callerRings: ReadonlyArray<Flows.Msg>,
  calleeRings: ReadonlyArray<Flows.Msg>,
  answerMs = 1_000
): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        ...callerRings,
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: answerMs, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: answerMs + 5, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: answerMs + 3_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: answerMs + 3_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        ...calleeRings,
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: answerMs - 10, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: answerMs + 10, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: answerMs + 3_010, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: answerMs + 3_015, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )

const ringsOn = (flow: ReturnType<typeof flowOf>, leg: string) =>
  flow.steps.filter((s) => s.leg === leg && s.op === "expect" && s.msg.status === 180)

const FLAG = "provisional-expect-surplus-tolerated"

describe("a caller-facing provisional with no peer emission behind it (§6.9)", () => {
  it("tolerates the platform's duplicate emission, 0.79 ms apart with a header stripped", () => {
    // One callee ring; the platform puts two on the caller leg, the second
    // stripped of the identity the first carried.
    const flow = flowOf(
      ringingCall(
        [
          callerRing(200, ["P-Asserted-Identity: <sip:ringing@example.invalid>"]),
          callerRing(201)
        ],
        [calleeRing(190)]
      )
    )
    const rings = ringsOn(flow, "A")
    expect(rings.length).toBe(2)
    expect(rings.map((s) => s.optional ?? false)).toEqual([false, true])
  })

  it("tolerates the platform re-sending its own unreliable 18x with nothing behind it", () => {
    const flow = flowOf(
      ringingCall([callerRing(200), callerRing(700, undefined, true)], [calleeRing(190)])
    )
    expect(ringsOn(flow, "A").map((s) => s.optional ?? false)).toEqual([false, true])
  })

  it("stamps the surplus at the END of the run, where another status can release it", () => {
    // Three caller rings for two callee rings: the SPARE is the second, but the
    // tolerance rides the third — §6.5 releases an optional only when a LATER
    // step on the leg matches, and a sibling of the same status is not one.
    const flow = flowOf(
      ringingCall(
        [callerRing(200), callerRing(201), callerRing(600)],
        [calleeRing(190), calleeRing(590)]
      )
    )
    expect(ringsOn(flow, "A").map((s) => s.optional ?? false)).toEqual([false, false, true])
  })

  it("tolerates a MINTED surplus too: what caused it is the anchor, not the check mode", () => {
    // The platform's own re-send lands far enough from the callee ring that the
    // classifier calls it SUT-originated (`check: "record"`); it is still a
    // caller-facing provisional with nothing behind it.
    const flow = flowOf(
      ringingCall([callerRing(200), callerRing(7_100, undefined, true)], [calleeRing(190)], 9_000)
    )
    const rings = ringsOn(flow, "A")
    expect(rings[1]!.check).toBe("record")
    expect(rings[1]!.optional).toBe(true)
  })

  it("leaves a leg that relayed one for one entirely alone", () => {
    const flow = flowOf(ringingCall([callerRing(200)], [calleeRing(190)]))
    expect(ringsOn(flow, "A").map((s) => s.optional ?? false)).toEqual([false])
    expect(flow.flags.map((f) => f.kind)).not.toContain(FLAG)
  })

  it("never tolerates the whole run: a leg that rang must still ring", () => {
    const flow = flowOf(ringingCall([callerRing(200), callerRing(201)], [calleeRing(190)]))
    const rings = ringsOn(flow, "A")
    expect(rings.filter((s) => s.optional === true).length).toBeLessThan(rings.length)
  })

  it("says which step it tolerated, on which leg, and out of what run", () => {
    const flow = flowOf(ringingCall([callerRing(200), callerRing(201)], [calleeRing(190)]))
    const flag = flow.flags.find((f) => f.kind === FLAG)!
    const spared = ringsOn(flow, "A").find((s) => s.optional === true)!
    expect(flag.detail).toContain(`${spared.id} (leg A, 180, 1 of a 2-step run)`)
  })
})
