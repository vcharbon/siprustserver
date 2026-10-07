/**
 * A relayed provisional is paired with the emission it relays by CONTENT —
 * status, body, To-tag — never by capture time alone, and its expectation is
 * placed after its source.
 *
 * A B2BUA may put two provisionals out in another order than it received them,
 * and one capture point can stamp a relay a little before the datagram it
 * relays when the two directions are captured on different interfaces. Capture
 * order then pairs an arrival with the wrong emission, or with none.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { synthesize } from "../src/flowsteps.js"
import { build } from "../src/topology.js"
import type { Vantage } from "../src/selection.js"
import {
  ANSWER_SDP,
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

const BOTH_VANTAGES: ReadonlyArray<Vantage> = [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }]
const { caller, callee, sut } = SOCKETS

const flowOf = (flows: Flows.FlowsDoc) =>
  synthesize(flows, build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix), plan())

/** A caller-facing 183, with an SDP answer or bare. */
const callerProgress = (ts_ms: number, sdp: boolean): Flows.Msg =>
  response({
    callId: CALLER_CALL_ID, seq: 1, status: 183, reason: "Session Progress", cseqMethod: "INVITE",
    src: sut, dst: caller, ts_ms, toTag: "sut-tag", ...(sdp ? { sdp: ANSWER_SDP } : {})
  })

/** A callee-facing 183, with an SDP answer or bare. */
const calleeProgress = (ts_ms: number, sdp: boolean): Flows.Msg =>
  response({
    callId: CALLEE_CALL_ID, seq: 1, status: 183, reason: "Session Progress", cseqMethod: "INVITE",
    src: callee, dst: sut, ts_ms, toTag: "callee-tag", ...(sdp ? { sdp: ANSWER_SDP } : {})
  })

/** An answered call whose provisionals cross the vantage as given. */
const call = (callerSide: ReadonlyArray<Flows.Msg>, calleeSide: ReadonlyArray<Flows.Msg>, answerMs = 5_000) =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        ...callerSide,
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: answerMs, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: answerMs + 5, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: answerMs + 3_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: answerMs + 3_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        ...calleeSide,
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: answerMs - 10, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: answerMs + 10, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: answerMs + 3_010, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: answerMs + 3_015, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )

type Flow = ReturnType<typeof flowOf>
type Step = Flow["steps"][number]

const progress = (flow: Flow, legId: string, op: "send" | "expect"): Array<Step> =>
  flow.steps.filter((s) => s.leg === legId && s.op === op && s.msg.status === 183)

const hasBody = (s: Step): boolean =>
  s.msg.body !== undefined && !("mode" in s.msg.body && s.msg.body.mode === "absent")

describe("two provisionals relayed out of arrival order", () => {
  /**
   * The callee sends a 183 with an answer, then a bare 183, 0.05 ms apart; the
   * platform relays the bare one first. Each relay is the one carrying the
   * same content.
   */
  const flows = call(
    [callerProgress(374.7, false), callerProgress(375.6, true)],
    [calleeProgress(373.4, true), calleeProgress(373.45, false)]
  )

  it("anchors each relayed arrival on the emission carrying its content", () => {
    const flow = flowOf(flows)
    const sends = progress(flow, "B", "send")
    const arrivals = progress(flow, "A", "expect")
    expect(sends.map(hasBody)).toEqual([true, false])
    const anchorOf = (withBody: boolean) => arrivals.find((s) => hasBody(s) === withBody)!.delay.from
    expect(anchorOf(true)).toBe(`step:${sends[0]!.id}`)
    expect(anchorOf(false)).toBe(`step:${sends[1]!.id}`)
  })

  it("places each relayed arrival after its own source, in the order of the sources", () => {
    const flow = flowOf(flows)
    const arrivals = progress(flow, "A", "expect")
    expect(arrivals.map(hasBody)).toEqual([true, false])
  })
})

describe("a relay stamped before the datagram it relays", () => {
  /**
   * The caller-side 183 is stamped 120 ms before the callee-side 183 it
   * relays (the two directions captured on different interfaces). The
   * arrival is still that emission's relay, and the document expects it only
   * after the callee sent it.
   */
  const flows = call([callerProgress(880, true)], [calleeProgress(1_000, true)])

  it("anchors the arrival on the emission it relays, and lists it after that emission", () => {
    const flow = flowOf(flows)
    const [send] = progress(flow, "B", "send")
    const arrivals = progress(flow, "A", "expect")
    expect(arrivals).toHaveLength(1)
    expect(arrivals[0]!.delay.from).toBe(`step:${send!.id}`)
    expect(flow.steps.indexOf(arrivals[0]!)).toBeGreaterThan(flow.steps.indexOf(send!))
  })
})
