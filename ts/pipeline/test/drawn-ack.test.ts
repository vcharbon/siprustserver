/**
 * An auto ACK expectation carries the count it DRAWS, not the ACKs the capture
 * held. The ACK to a 2xx is the acknowledging peer's own, relayed (RFC 3261
 * §13.2.2.4), so a copy of the final inside the wait for it draws nothing and a
 * copy after it draws one, until a later INVITE transaction resets the retained
 * datagram. The ACK to a non-2xx final is the client transaction's own
 * (§17.1.1.3), composed on arrival, one per copy. Nothing on the far leg draws
 * one. What separates the two is the FINAL's status — never the body the ACK
 * carries, nor whether it confirms the dialog.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import type { StepDraft } from "../src/draft.js"
import { stampDrawnAckCounts } from "../src/drawn-ack.js"
import { synthesize, type StepSource } from "../src/flowsteps.js"
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

/**
 * A plain answered call at both vantages. `calleeFinalRepeats` copies of the
 * callee's 200 reach the platform, which passes the caller's single ACK on
 * unchanged — the under-ACKing shape a capture of a non-compliant platform
 * holds.
 *
 * The options place the b-leg ACK (`ackAtMs`), give it a body (`ackBody`), and
 * repeat the CALLER's final and its ACK so the platform has something to relay.
 */
const answeredCall = (
  calleeFinalRepeats: number,
  opts: {
    readonly ackAtMs?: number
    readonly ackBody?: { readonly contentType: string; readonly text: string }
    readonly callerFinalRepeats?: number
    readonly callerAckRepeats?: number
  } = {}
): Flows.FlowsDoc => {
  const { caller, callee, sut } = SOCKETS
  const ackAtMs = opts.ackAtMs ?? 3_000
  const calleeOk = (ts_ms: number): Flows.Msg =>
    response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms, toTag: "callee-tag" })
  const repeats = Array.from({ length: calleeFinalRepeats }, (_, n) => ({
    ...calleeOk(990 + (n + 1) * 500),
    retx: true,
    probe: 0,
    repeat_of: 3
  }))
  const callerOk = (ts_ms: number): Flows.Msg =>
    response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms, toTag: "sut-tag" })
  const callerAck = (ts_ms: number): Flows.Msg =>
    request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms, toTag: "sut-tag" })
  // In wire order: the final and its repeats, then the ACK and its own.
  const callerOkRepeats = Array.from({ length: opts.callerFinalRepeats ?? 0 }, (_, n) => ({
    ...callerOk(1_000 + (n + 1) * 400),
    retx: true,
    probe: 0,
    repeat_of: 3
  }))
  const callerAckRepeats = Array.from({ length: opts.callerAckRepeats ?? 0 }, (_, n) => ({
    ...callerAck(1_600 + (n + 1) * 20),
    retx: true,
    probe: 0,
    repeat_of: 4 + callerOkRepeats.length
  }))
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag" }),
        callerOk(1_000),
        ...callerOkRepeats,
        callerAck(1_600),
        ...callerAckRepeats,
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 4_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 4_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: "callee-tag" }),
        calleeOk(990),
        ...repeats,
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: ackAtMs, toTag: "callee-tag", ...(opts.ackBody === undefined ? {} : { body: opts.ackBody }) }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 4_010, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 4_015, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/**
 * The same call, renegotiated: after the initial ACK the caller re-INVITEs, the
 * callee answers with `reinviteFinalRepeats` copies of its 200, and the b-leg
 * ACK for that transaction goes out at `reAckAtMs`: the caller's own ACK of
 * that 200, relayed, exactly as on the initial round.
 */
const renegotiatedCall = (
  reinviteFinalRepeats: number,
  opts: { readonly reAckAtMs?: number } = {}
): Flows.FlowsDoc => {
  const { caller, callee, sut } = SOCKETS
  const reAckAtMs = opts.reAckAtMs ?? 3_600
  const calleeReOk = (ts_ms: number): Flows.Msg =>
    response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms, toTag: "callee-tag" })
  // Index 6 on the callee leg: the re-INVITE's own final, which the repeats echo.
  const reOkRepeats = Array.from({ length: reinviteFinalRepeats }, (_, n) => ({
    ...calleeReOk(2_100 + (n + 1) * 500),
    retx: true,
    probe: 0,
    repeat_of: 6
  }))
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_600, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "INVITE", src: caller, dst: sut, ts_ms: 2_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 2_150, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "ACK", src: caller, dst: sut, ts_ms: 3_500, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "BYE", src: caller, dst: sut, ts_ms: 4_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 4_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 990, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_700, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "INVITE", src: sut, dst: callee, ts_ms: 2_010, toTag: "callee-tag" }),
        calleeReOk(2_100),
        ...reOkRepeats,
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "ACK", src: sut, dst: callee, ts_ms: reAckAtMs, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 3, method: "BYE", src: sut, dst: callee, ts_ms: 4_010, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 4_015, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/**
 * The initial 200 repeated LATE — once the caller's re-INVITE has already
 * opened a second INVITE transaction on the callee leg, which is the reset that
 * drops the retained ACK the copy would otherwise draw (§6.1).
 */
const supersededFinalCall = (): Flows.FlowsDoc => {
  const { caller, callee, sut } = SOCKETS
  const calleeOk = (ts_ms: number): Flows.Msg =>
    response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms, toTag: "callee-tag" })
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_100, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "INVITE", src: caller, dst: sut, ts_ms: 1_190, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_360, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "ACK", src: caller, dst: sut, ts_ms: 1_400, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 3, method: "BYE", src: caller, dst: sut, ts_ms: 4_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 4_005, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: "callee-tag" }),
        calleeOk(990),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_010, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "INVITE", src: sut, dst: callee, ts_ms: 1_200, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 1_350, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "ACK", src: sut, dst: callee, ts_ms: 1_410, toTag: "callee-tag" }),
        { ...calleeOk(1_490), retx: true, probe: 0, repeat_of: 3 },
        request({ callId: CALLEE_CALL_ID, seq: 3, method: "BYE", src: sut, dst: callee, ts_ms: 4_010, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 4_015, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

/**
 * The same dial REJECTED: the callee answers 486 and repeats it
 * `finalRepeats` times, and the platform ACKs each copy from the final itself
 * (RFC 3261 §17.1.1.3). The b-leg ACK the capture holds sits at `ackAtMs`.
 */
const rejectedCall = (
  finalRepeats: number,
  opts: { readonly ackAtMs?: number } = {}
): Flows.FlowsDoc => {
  const { caller, callee, sut } = SOCKETS
  const ackAtMs = opts.ackAtMs ?? 3_000
  const calleeBusy = (ts_ms: number): Flows.Msg =>
    response({ callId: CALLEE_CALL_ID, seq: 1, status: 486, reason: "Busy Here", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms, toTag: "callee-tag" })
  const repeats = Array.from({ length: finalRepeats }, (_, n) => ({
    ...calleeBusy(990 + (n + 1) * 500),
    retx: true,
    probe: 0,
    repeat_of: 3
  }))
  return doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 486, reason: "Busy Here", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_100, toTag: "sut-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: "callee-tag" }),
        calleeBusy(990),
        ...repeats,
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: ackAtMs, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

const ackOn = (flow: ReturnType<typeof flowOf>, leg: string, op: "send" | "expect") =>
  flow.steps.find(
    (s) => s.leg === leg && s.op === op && (s.msg.method ?? "").toUpperCase() === "ACK"
  )

const reAckOn = (flow: ReturnType<typeof flowOf>) =>
  flow.steps
    .filter((s) => s.leg === "B" && s.op === "expect" && (s.msg.method ?? "").toUpperCase() === "ACK")
    .at(-1)

describe("the ACK to a 2xx is the peer's own, relayed (§6.3)", () => {
  it("draws nothing from the copies the wait for that ACK swallowed", () => {
    // Copies at 1490 and 1990; the b-leg ACK the capture holds is at 3000.
    const flow = flowOf(answeredCall(2))
    const finalOk = flow.steps.find((s) => s.leg === "B" && s.op === "send" && s.msg.status === 200)
    // The peer's own ladder stays as captured; only what it DRAWS is read anew.
    expect(finalOk?.retransmits).toBe(2)
    expect(ackOn(flow, "B", "expect")?.retransmits).toBeUndefined()
    expect(flow.flags.map((f) => f.kind)).not.toContain("ack-count-drawn-from-final")
  })

  it("draws one re-send per copy landing once that ACK exists", () => {
    // Copies at 1490 and 1990; the ACK goes out between them.
    expect(ackOn(flowOf(answeredCall(2, { ackAtMs: 1_700 })), "B", "expect")?.retransmits).toBe(1)
  })

  it("scales with the copies past the ACK", () => {
    // Copies at 1490, 1990 and 2490: two of them land after the ACK.
    expect(ackOn(flowOf(answeredCall(3, { ackAtMs: 1_700 })), "B", "expect")?.retransmits).toBe(2)
  })

  it("leaves an ACK whose final was never repeated without a count", () => {
    const flow = flowOf(answeredCall(0))
    expect(ackOn(flow, "B", "expect")?.retransmits).toBeUndefined()
    expect(flow.flags.map((f) => f.kind)).not.toContain("ack-count-drawn-from-final")
  })

  it("leaves the SEND half exactly as the capture held it", () => {
    // The caller's own ACK is the scripted actor's behaviour, not the peer's.
    const flow = flowOf(answeredCall(1))
    expect(ackOn(flow, "A", "send")?.retransmits).toBeUndefined()
  })

  it("says which step took which count, and what the capture held", () => {
    const flow = flowOf(answeredCall(2, { ackAtMs: 1_700 }))
    const flag = flow.flags.find((f) => f.kind === "ack-count-drawn-from-final")!
    const ack = ackOn(flow, "B", "expect")!
    expect(flag.detail).toContain(`${ack.id} (leg B,`)
    expect(flag.detail).toContain("1 drawn against")
  })

  it("draws nothing for a copy landing once a later INVITE transaction has opened", () => {
    const flow = flowOf(supersededFinalCall())
    const finalOk = flow.steps.find(
      (s) => s.leg === "B" && s.op === "send" && s.msg.status === 200 && s.retransmits !== undefined
    )
    expect(finalOk?.retransmits).toBe(1)
    const initialAck = flow.steps.find(
      (s) => s.leg === "B" && s.op === "expect" && (s.msg.method ?? "") === "ACK" && s.msg.cseq === 1
    )
    expect(initialAck?.retransmits).toBeUndefined()
  })
})

describe("a re-INVITE's 2xx is acknowledged the same way", () => {
  // The re-INVITE's ACK confirms nothing — the dialog was confirmed by the
  // initial one — and is still the caller's own, relayed.
  it("draws the copies past the relayed ACK and none before it", () => {
    // Copies at 2600 and 3100; the b-leg ACK goes out between them.
    const flow = flowOf(renegotiatedCall(2, { reAckAtMs: 2_800 }))
    const reAck = reAckOn(flow)
    expect(reAck?.confirms_dialog).toBeUndefined()
    expect(reAck?.retransmits).toBe(1)
  })

  it("draws nothing where the whole ladder lands inside the wait", () => {
    // Copies at 2600 and 3100; the ACK the capture holds is at 3600.
    expect(reAckOn(flowOf(renegotiatedCall(2)))?.retransmits).toBeUndefined()
  })

  it("names the re-INVITE's own final as the one it drew against", () => {
    const flow = flowOf(renegotiatedCall(2, { reAckAtMs: 2_800 }))
    const flag = flow.flags.find((f) => f.kind === "ack-count-drawn-from-final")!
    expect(flag.detail).toContain(`${reAckOn(flow)!.id} (leg B,`)
    expect(flag.detail).toContain("1 drawn against")
  })
})

describe("the body the ACK carries decides nothing", () => {
  // A delayed offer's answer rides the ACK (RFC 3261 §13.2.1) and a closed
  // round's ACK may carry a payload no stack interprets: either way the ACK is
  // the acknowledging peer's own, and the count is the same.
  const answer = { contentType: "application/sdp", text: "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\n" }

  it("draws what a bodyless ACK draws, copy for copy", () => {
    const bodied = flowOf(answeredCall(2, { ackAtMs: 1_700, ackBody: answer }))
    const bare = flowOf(answeredCall(2, { ackAtMs: 1_700 }))
    expect(ackOn(bodied, "B", "expect")?.retransmits).toBe(1)
    expect(ackOn(bare, "B", "expect")?.retransmits).toBe(1)
  })

  it("draws nothing from the copies the wait swallowed, body or none", () => {
    expect(ackOn(flowOf(answeredCall(2, { ackBody: answer })), "B", "expect")?.retransmits)
      .toBeUndefined()
    expect(ackOn(flowOf(answeredCall(2)), "B", "expect")?.retransmits).toBeUndefined()
  })
})

describe("an ACK to a NON-2xx final is the client transaction's own (§17.1.1.3)", () => {
  // Composed from the final itself, hop by hop, so every copy draws one
  // whenever the capture's single ACK happened to go out.
  it("draws one per copy of the ladder", () => {
    expect(ackOn(flowOf(rejectedCall(2)), "B", "expect")?.retransmits).toBe(2)
  })

  it("draws them whether the capture's ACK is early or late", () => {
    expect(ackOn(flowOf(rejectedCall(2, { ackAtMs: 1_010 })), "B", "expect")?.retransmits).toBe(2)
  })

  it("leaves an ACK whose final was never repeated without a count", () => {
    const flow = flowOf(rejectedCall(0))
    expect(ackOn(flow, "B", "expect")?.retransmits).toBeUndefined()
    expect(flow.flags.map((f) => f.kind)).not.toContain("ack-count-drawn-from-final")
  })
})

describe("an ACK the far leg sends draws none on this one", () => {
  // A B2BUA owes one ACK per final it RECEIVES on a leg, so a caller that ACKs
  // twice for one answer puts nothing extra on the callee's leg.
  it("leaves the expectation uncounted where its own final drew nothing", () => {
    const flow = flowOf(answeredCall(0, { callerFinalRepeats: 1, callerAckRepeats: 1 }))
    expect(ackOn(flow, "A", "send")?.retransmits).toBe(1)
    expect(ackOn(flow, "B", "expect")?.retransmits).toBeUndefined()
  })

  it("raises no flag, because the count the capture held already stands", () => {
    const flow = flowOf(answeredCall(0, { callerFinalRepeats: 1, callerAckRepeats: 1 }))
    expect(flow.flags.map((f) => f.kind)).not.toContain("ack-count-drawn-from-final")
  })

  it("still draws the callee ladder's own copies alongside a repeated caller ACK", () => {
    const flow = flowOf(
      answeredCall(2, { ackAtMs: 1_700, callerFinalRepeats: 1, callerAckRepeats: 1 })
    )
    expect(ackOn(flow, "B", "expect")?.retransmits).toBe(1)
  })
})

/**
 * The same document, re-stamped after the callee final's MEASURED gaps are
 * struck: the shape an authored document has (§6.9 — `retransmits` says how
 * many, nothing says when), which paces the final by the class's own schedule
 * (`@sip/contracts/schedules`, the table `sip-retransmit` walks) instead.
 * `strike` removes anything else the case needs gone.
 */
const restampedWithoutMeasuredGaps = (
  flows: Flows.FlowsDoc,
  strike: (steps: Array<StepDraft>) => void = () => {}
) => {
  const flow = flowOf(flows)
  const steps: Array<StepDraft> = flow.steps.map((s) => ({ ...s }))
  const sources: Array<StepSource> = steps.map((s) => ({
    id: s.id,
    pivotLeg: s.leg,
    origLeg: s.observed!.leg,
    msgIdx: s.observed!.msg,
    emits: s.op === "send",
    auto: s.auto === true
  }))
  for (const s of steps) {
    if (s.leg === "B" && s.op === "send" && s.msg.status === 200) delete s.retransmit_intervals_ms
    if (s.leg === "B" && s.op === "expect" && (s.msg.method ?? "") === "ACK") delete s.retransmits
  }
  strike(steps)
  const drawn = stampDrawnAckCounts(flows, steps, sources)
  return { steps, drawn }
}

const restampedAck = (steps: ReadonlyArray<StepDraft>) =>
  steps.find((s) => s.leg === "B" && s.op === "expect" && (s.msg.method ?? "") === "ACK")

describe("a final stating no measured gaps is paced by its class's schedule", () => {
  it("holds the copies the RFC's rungs put inside the wait, not the capture's", () => {
    // As captured, both copies (500 and 1000 ms behind the final) land inside
    // the 1010 ms wait for the peer's ACK: nothing is drawn.
    const measured = flowOf(answeredCall(2, { ackAtMs: 2_000 }))
    expect(ackOn(measured, "B", "expect")?.retransmits).toBeUndefined()
    // On the `final-2xx` schedule the rungs land at 500 and 1500 ms: the
    // second is past the wait, and draws a re-send of that ACK.
    const { steps, drawn } = restampedWithoutMeasuredGaps(answeredCall(2, { ackAtMs: 2_000 }))
    const ack = restampedAck(steps)
    expect(ack?.retransmits).toBe(1)
    expect(drawn.map((d) => [d.step, d.drawn])).toEqual([[ack!.id, 1]])
  })

  it("holds nothing where the document states no coordinate to compare", () => {
    // Without the two `observed` instants there is no wait to measure, so
    // every copy of the ladder draws.
    const { steps } = restampedWithoutMeasuredGaps(
      answeredCall(2, { ackAtMs: 2_000 }),
      (s) => {
        const ack = restampedAck(s)
        if (ack !== undefined) delete ack.observed
      }
    )
    expect(restampedAck(steps)?.retransmits).toBe(2)
  })

  it("displaces a copy the schedule lands past the next INVITE, as the measured gap did", () => {
    const { steps } = restampedWithoutMeasuredGaps(supersededFinalCall())
    const initialAck = steps.find(
      (s) => s.leg === "B" && s.op === "expect" && (s.msg.method ?? "") === "ACK" && s.msg.cseq === 1
    )
    expect(initialAck?.retransmits).toBeUndefined()
  })
})
