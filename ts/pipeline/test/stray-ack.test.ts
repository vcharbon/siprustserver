/**
 * The ACK to a non-2xx final is the INVITE client transaction's own (RFC 3261
 * §17.1.1.3): one per final, on the INVITE's branch, composed by the replay's
 * stack. A captured peer that also sent ACKs to that final on fresh branches
 * sent requests that ride no transaction; scripted as further auto ACKs they
 * would all leave on the INVITE's branch with different bytes, a rung
 * divergence the peer never committed. The cut keeps the transaction's own ACK
 * of the final on each hop and drops the others, with a flag.
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
  sutSet,
  withoutRepeatOf
} from "./fixtures.js"

const BOTH_VANTAGES: ReadonlyArray<Vantage> = [
  { leg: 0, hop: 0 },
  { leg: 1, hop: 0 }
]

const INVITE_BRANCH = `z9hG4bK-${CALLER_CALL_ID}-1-INVITE`

/**
 * A cancelled call whose caller answers the platform's 487 with the
 * transaction's ACK and with `strays` more ACKs on fresh branches, each
 * carrying a header the transaction's ACK does not.
 */
const cancelledWithStrayAcks = (strays: number, transactionAck = true): Flows.FlowsDoc => {
  const { caller, callee, sut } = SOCKETS
  const a = CALLER_CALL_ID
  const b = CALLEE_CALL_ID
  const stray = (i: number): Flows.Msg =>
    request({
      callId: a, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_110 + i, toTag: "sut-tag",
      branch: `z9hG4bK-stray-${i}`, headers: ["User-Agent: edge"]
    })
  return doc(
    [
      leg(a, oneHop(caller, sut), [
        request({ callId: a, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: a, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 5 }),
        response({ callId: a, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag" }),
        request({ callId: a, seq: 1, method: "CANCEL", src: caller, dst: sut, ts_ms: 1_000, branch: INVITE_BRANCH }),
        response({ callId: a, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: sut, dst: caller, ts_ms: 1_010, toTag: "sut-tag", branch: INVITE_BRANCH }),
        response({ callId: a, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_100, toTag: "sut-tag" }),
        stray(0),
        ...(transactionAck
          ? [request({ callId: a, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_105 + strays, toTag: "sut-tag", branch: INVITE_BRANCH })]
          : []),
        ...Array.from({ length: strays - 1 }, (_, i) => stray(i + 1))
      ]),
      leg(b, oneHop(sut, callee), [
        request({ callId: b, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
        response({ callId: b, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 15 }),
        response({ callId: b, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 190, toTag: "callee-tag" }),
        request({ callId: b, seq: 1, method: "CANCEL", src: sut, dst: callee, ts_ms: 1_020 }),
        response({ callId: b, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: callee, dst: sut, ts_ms: 1_025, toTag: "callee-tag" }),
        response({ callId: b, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 1_060, toTag: "callee-tag" }),
        request({ callId: b, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_065, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )
}

const flowOf = (flows: Flows.FlowsDoc) =>
  synthesize(flows, build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix), plan())

const callerAcks = (flows: Flows.FlowsDoc) => {
  const flow = flowOf(flows)
  const acks = flow.steps.filter((s) => s.msg.method === "ACK" && s.op === "send")
  return { flow, acks }
}

describe("ACKs to a non-2xx final on fresh branches (RFC 3261 §17.1.1.3)", () => {
  it("keeps the transaction's own ACK and drops the strays, with a flag", () => {
    const flows = cancelledWithStrayAcks(3)
    const { flow, acks } = callerAcks(flows)
    expect(acks).toHaveLength(1)
    const kept = flows.legs[0]!.msgs[acks[0]!.observed!.msg]!
    expect(kept.via?.[0]?.branch).toBe(INVITE_BRANCH)
    const flag = flow.flags.find((f) => f.kind === "ack-off-transaction-dropped")
    expect(flag?.detail).toContain("3 captured ACK(s)")
  })

  it("keeps the first where none rides the final's branch", () => {
    const flows = cancelledWithStrayAcks(2, false)
    const { acks } = callerAcks(flows)
    expect(acks).toHaveLength(1)
    expect(acks[0]!.observed!.msg).toBe(6)
  })

  it("drops nothing where the final drew one ACK", () => {
    const { flow, acks } = callerAcks(cancelledWithStrayAcks(1, false))
    expect(acks).toHaveLength(1)
    expect(flow.flags.some((f) => f.kind === "ack-off-transaction-dropped")).toBe(false)
  })

  it("groups an ACK to a retransmitted final with the final it repeats", () => {
    // The 487 is sent twice; the transaction's ACK answers the first copy and
    // a stray on a fresh branch answers the second.
    const flows = cancelledWithStrayAcks(1)
    const msgs = [...flows.legs[0]!.msgs]
    const final = msgs[5]!
    const ack = msgs[7]!
    const stray = msgs[6]!
    msgs.splice(6, 2, ack, { ...final, ts_us: final.ts_us + 500_000, retx: true, repeat_of: 5 }, {
      ...stray,
      ts_us: final.ts_us + 501_000
    })
    const regrouped = { ...flows, legs: [{ ...flows.legs[0]!, msgs }, flows.legs[1]!] }
    const { acks } = callerAcks(regrouped)
    expect(acks).toHaveLength(1)
    expect(regrouped.legs[0]!.msgs[acks[0]!.observed!.msg]!.via?.[0]?.branch).toBe(INVITE_BRANCH)
  })

  it("never counts a stray's retransmission as a rung of the kept ACK", () => {
    // A legacy document (no repeat_of): the stray is sent twice on its branch.
    const flows = cancelledWithStrayAcks(1)
    const msgs = [...flows.legs[0]!.msgs]
    const stray = msgs[6]!
    msgs.push({ ...stray, ts_us: msgs[7]!.ts_us + 1_000, retx: true })
    const legacy = withoutRepeatOf({ ...flows, legs: [{ ...flows.legs[0]!, msgs }, flows.legs[1]!] })
    const { acks } = callerAcks(legacy)
    expect(acks).toHaveLength(1)
    expect(acks[0]!.retransmits ?? 0).toBe(0)
  })
})
