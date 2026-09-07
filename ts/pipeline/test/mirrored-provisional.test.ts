/**
 * A relayed provisional crosses the vantage twice, and the document says the
 * same thing about both halves: where the captured platform dropped one on its
 * own account, the emission still gets the arrival a relaying B2BUA gives it
 * (§6.9, issue 116).
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

const BOTH_VANTAGES: ReadonlyArray<Vantage> = [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }]
const { caller, callee, sut } = SOCKETS

const layoutOf = (flows: Flows.FlowsDoc) =>
  build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix)

/** The default reading: the captured platform relayed as it received. */
const flowOf = (flows: Flows.FlowsDoc) => synthesize(flows, layoutOf(flows), plan())

/** The same capture read as a platform running a rewrite mode. */
const rewritingFlowOf = (flows: Flows.FlowsDoc) =>
  synthesize(flows, layoutOf(flows), plan(), undefined, undefined, undefined, false)

const callerRing = (ts_ms: number, status = 180): Flows.Msg =>
  response({
    callId: CALLER_CALL_ID, seq: 1, status, reason: "Ringing", cseqMethod: "INVITE",
    src: sut, dst: caller, ts_ms, toTag: "sut-tag"
  })

/** One callee-facing ring. `rseq` makes it the RELIABLE class, which retransmits. */
const calleeRing = (
  ts_ms: number,
  rseq?: number,
  headers?: ReadonlyArray<string>,
  status = 180
): Flows.Msg =>
  response({
    callId: CALLEE_CALL_ID, seq: 1, status, reason: "Ringing", cseqMethod: "INVITE",
    src: callee, dst: sut, ts_ms, toTag: "callee-tag",
    ...(rseq === undefined ? {} : { headers: [`RSeq: ${rseq}`, "Require: 100rel"] }),
    ...(headers === undefined ? {} : { headers })
  })

const ringingCall = (
  callerRings: ReadonlyArray<Flows.Msg>,
  calleeRings: ReadonlyArray<Flows.Msg>,
  answerMs = 5_000
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

const ringsOn = (
  flow: ReturnType<typeof flowOf>,
  leg: string,
  op: "send" | "expect",
  status = 180
) => flow.steps.filter((s) => s.leg === leg && s.op === op && s.msg.status === status)

const FLAG = "relayed-provisional-expect-derived"
const flagOf = (flow: ReturnType<typeof flowOf>) => flow.flags.find((f) => f.kind === FLAG)

describe("an emission the capture holds no relay of (§6.9)", () => {
  /** capture_102648: two callee rings 402 ms apart, one caller ring. */
  const REFERENCE = ringingCall([callerRing(277)], [calleeRing(268), calleeRing(671)])

  it("gives it the arrival a relaying B2BUA sends, so both legs carry two", () => {
    const flow = flowOf(REFERENCE)
    expect(ringsOn(flow, "B", "send").length).toBe(2)
    expect(ringsOn(flow, "A", "expect").length).toBe(2)
  })

  it("anchors the derived arrival on the emission it relays", () => {
    const flow = flowOf(REFERENCE)
    const rings = ringsOn(flow, "A", "expect")
    const sends = ringsOn(flow, "B", "send")
    expect(rings[1]!.delay.from).toBe(`step:${sends[1]!.id}`)
    expect(rings[1]!.delay.ms).toBe(0)
  })

  it("copies the captured arrival, `observed` coordinate included", () => {
    // The SUT emits that captured message again, so it is what BOTH datagrams
    // are compared against — and the second keeps its header comparison.
    const flow = flowOf(REFERENCE)
    const rings = ringsOn(flow, "A", "expect")
    expect(rings[1]!.observed).toEqual(rings[0]!.observed)
    expect(rings[1]!.msg).toEqual(rings[0]!.msg)
    expect(rings[1]!.check).toBe(rings[0]!.check)
  })

  it("names every derivation in the flag", () => {
    const flow = flowOf(REFERENCE)
    const flag = flagOf(flow)
    expect(flag).toBeDefined()
    expect(flag!.detail).toContain(ringsOn(flow, "A", "expect")[1]!.id)
  })

  it("renumbers the flow, so ids stay positional and every anchor resolves", () => {
    const flow = flowOf(REFERENCE)
    expect(flow.steps.map((s) => s.id)).toEqual(flow.steps.map((_, i) => `s${i + 1}`))
    expect(flow.sources.map((s) => s.id)).toEqual(flow.steps.map((s) => s.id))
    const ids = new Set(flow.steps.map((s) => s.id))
    for (const step of flow.steps) {
      if (step.delay.from.startsWith("step:")) {
        expect(ids.has(step.delay.from.slice(5))).toBe(true)
      }
    }
  })

  it("treats a 183 exactly as a 180 — the class is the rule, never the status", () => {
    // The same shape one status over. An unreliable provisional is one whatever
    // it is called, so the reference case must compile identically.
    const flow = flowOf(
      ringingCall(
        [callerRing(277, 183)],
        [calleeRing(268, undefined, undefined, 183), calleeRing(671, undefined, undefined, 183)]
      )
    )
    const sends = ringsOn(flow, "B", "send", 183)
    const arrivals = ringsOn(flow, "A", "expect", 183)
    expect(sends.length).toBe(2)
    expect(arrivals.length).toBe(2)
    expect(arrivals[1]!.observed).toEqual(arrivals[0]!.observed)
    expect(arrivals[1]!.delay.from).toBe(`step:${sends[1]!.id}`)
    expect(flagOf(flow)!.detail).toContain("183")
  })

  it("derives nothing when two emissions milliseconds apart were BOTH relayed", () => {
    // capture_159374's shape. `relayOriginOf` takes the LATEST emission in its
    // window, so both arrivals name the second send — and reading that as "the
    // first was never relayed" invents a third ring the SUT never sends.
    const flow = flowOf(
      ringingCall([callerRing(541), callerRing(546)], [calleeRing(536), calleeRing(541)])
    )
    expect(ringsOn(flow, "B", "send").length).toBe(2)
    expect(ringsOn(flow, "A", "expect").length).toBe(2)
    expect(flagOf(flow)).toBeUndefined()
  })

  it("resolves the arrival it copies AFTER the insertions that move it", () => {
    // Three emissions, one relayed: each insertion shifts the array under the
    // next, so a derivation resolving its copy by INDEX reads the wrong step.
    const flow = flowOf(
      ringingCall([callerRing(1_110)], [calleeRing(268), calleeRing(671), calleeRing(1_100)])
    )
    const rings = ringsOn(flow, "A", "expect")
    const sends = ringsOn(flow, "B", "send")
    expect(rings.length).toBe(3)
    rings.forEach((ring, i) => {
      expect(ring.msg.status).toBe(180)
      expect(ring.msg).toEqual(rings[2]!.msg)
      expect(ring.delay.from).toBe(`step:${sends[i]!.id}`)
    })
  })

  it("declines to predict a relay of a message the SUT was never seen handling", () => {
    // A bare ring then a ring authorising early media: two different relays, and
    // the capture holds one arrival. Copying the wrong one is worse than
    // leaving the deficit, so nothing is derived.
    const flow = flowOf(
      ringingCall([callerRing(277)], [calleeRing(268), calleeRing(671, undefined, ["P-Early-Media: sendrecv"])])
    )
    expect(ringsOn(flow, "A", "expect").length).toBe(1)
    expect(flagOf(flow)).toBeUndefined()
  })

  it("compares a stored body by CONTENT, not by the file its ref names", () => {
    // A ref names one file per captured emission, so two byte-identical bodies
    // sit under different names. Comparing the NAME declines a relay the SUT
    // reproduces exactly — which is most of the multi-ring population.
    const withSdp = (ts_ms: number): Flows.Msg =>
      response({
        callId: CALLEE_CALL_ID, seq: 1, status: 183, reason: "Session Progress",
        cseqMethod: "INVITE", src: callee, dst: sut, ts_ms, toTag: "callee-tag",
        sdp: "v=0\r\no=- 1 1 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n"
      })
    const flow = flowOf(
      ringingCall([callerRing(277, 183)], [withSdp(268), withSdp(671)])
    )
    const sends = ringsOn(flow, "B", "send", 183)
    expect(sends.length).toBe(2)
    // Two files, one body: different refs, same message.
    const refOf = (b: unknown): string | undefined =>
      typeof b === "object" && b !== null && "ref" in b ? (b as { ref: string }).ref : undefined
    expect(refOf(sends[0]!.msg.body)).toBeDefined()
    expect(refOf(sends[0]!.msg.body)).not.toBe(refOf(sends[1]!.msg.body))
    expect(ringsOn(flow, "A", "expect", 183).length).toBe(2)
    expect(flagOf(flow)).toBeDefined()
  })

  it("declines a form that differs only in a header VALUE, not in the header count", () => {
    // The comparison has to reach every level of the spec. A replacer-array
    // stringify erases header values and body refs, leaving little but the
    // count — and two rings authorising DIFFERENT early media read alike.
    const withPem = (ts_ms: number, value: string): Flows.Msg =>
      calleeRing(ts_ms, undefined, [`P-Early-Media: ${value}`])
    const flow = flowOf(
      ringingCall([callerRing(277)], [withPem(268, "sendrecv"), withPem(671, "inactive")])
    )
    expect(ringsOn(flow, "A", "expect").length).toBe(1)
    expect(flagOf(flow)).toBeUndefined()
  })

  it("classifies a reliable provisional the same on the leg that EXPECTS it", () => {
    // A send stores `RSeq`; an expect states `headers-present: ["rseq"]`, the
    // stack owning the value (§9.1). Reading one spelling calls the relayed
    // half of a reliable stream unreliable — the asymmetry this pass exists to
    // remove — and lets it claim an emission it does not answer.
    const reliableRelay = (ts_ms: number, rseq: number): Flows.Msg =>
      response({
        callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE",
        src: sut, dst: caller, ts_ms, toTag: "sut-tag",
        headers: [`RSeq: ${rseq}`, "Require: 100rel"]
      })
    const flow = flowOf(
      ringingCall([reliableRelay(277, 1)], [calleeRing(268, 1), calleeRing(671, 2)])
    )
    const arrivals = flow.steps.filter(
      (s) => s.leg === "A" && s.op === "expect" && s.msg.status === 180
    )
    expect(arrivals.length).toBe(1)
    expect(arrivals[0]!.msg["headers-present"]).toContain("rseq")
    // Reliable on both legs: RFC 3262 §3 paces it, so nothing here is derived.
    expect(flagOf(flow)).toBeUndefined()
  })

  it("derives nothing where the platform runs a rewrite mode", () => {
    // capture_205924's shape. A platform rewriting its 18x emits one by design,
    // the replaying SUT is driven the same way, and a derived second would gate
    // on a datagram nothing causes.
    const flow = rewritingFlowOf(REFERENCE)
    expect(ringsOn(flow, "A", "expect").length).toBe(1)
    expect(flagOf(flow)).toBeUndefined()
  })

  it("derives nothing for a RELIABLE provisional, whose repeats are a ladder", () => {
    const flow = flowOf(ringingCall([callerRing(277)], [calleeRing(268, 1), calleeRing(671, 1)]))
    expect(flagOf(flow)).toBeUndefined()
  })

  it("derives nothing for two DISTINCT reliable provisionals either", () => {
    // Different RSeq is two messages, not a ladder, so there ARE two sends —
    // and the relayed forms differ, so no arrival can be copied from the other.
    const flow = flowOf(ringingCall([callerRing(277)], [calleeRing(268, 1), calleeRing(671, 2)]))
    expect(ringsOn(flow, "B", "send").length).toBe(2)
    expect(flagOf(flow)).toBeUndefined()
  })

  it("leaves a leg that relayed one for one entirely alone", () => {
    const flow = flowOf(ringingCall([callerRing(277)], [calleeRing(268)]))
    expect(ringsOn(flow, "A", "expect").length).toBe(1)
    expect(flagOf(flow)).toBeUndefined()
  })

  it("derives nothing after the relaying leg has taken its own final", () => {
    // capture_f84347f4's shape: the caller CANCELs, the a-side transaction ends
    // on a 487, and the callee emits one last ring. A client transaction leaves
    // Proceeding on a final (RFC 3261 §17.1.1.2), so nothing relays it.
    const flows = doc(
      [
        leg(CALLER_CALL_ID, oneHop(caller, sut), [
          request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
          response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
          callerRing(200),
          request({ callId: CALLER_CALL_ID, seq: 1, method: "CANCEL", src: caller, dst: sut, ts_ms: 1_000 }),
          response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: sut, dst: caller, ts_ms: 1_005 }),
          response({ callId: CALLER_CALL_ID, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_010, toTag: "sut-tag" }),
          request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_012, toTag: "sut-tag" })
        ]),
        leg(CALLEE_CALL_ID, oneHop(sut, callee), [
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
          response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
          calleeRing(190),
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "CANCEL", src: sut, dst: callee, ts_ms: 1_006 }),
          response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: callee, dst: sut, ts_ms: 1_008 }),
          calleeRing(1_014),
          response({ callId: CALLEE_CALL_ID, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 1_090, toTag: "callee-tag" }),
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_092, toTag: "callee-tag" })
        ])
      ],
      [{ legs: [0, 1] }]
    )
    const flow = flowOf(flows)
    expect(ringsOn(flow, "B", "send").length).toBe(2)
    expect(ringsOn(flow, "A", "expect").length).toBe(1)
    expect(flagOf(flow)).toBeUndefined()
  })
})
