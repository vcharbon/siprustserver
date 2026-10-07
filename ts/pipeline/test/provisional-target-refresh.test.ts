/**
 * Two unreliable provisionals on one early dialog that differ only by their
 * Contact are two messages, not a retransmission: the second refreshes the
 * early dialog's remote target (RFC 3261 §12.1.2 / §13.2.2.4). The document
 * keeps that difference, or the replayed peer sends one datagram twice and the
 * system under test absorbs the second as a retransmission (§17.1.1).
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

const ring = (callId: string, src: string, dst: string, ts_ms: number, contact: string): Flows.Msg =>
  response({
    callId, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src, dst, ts_ms,
    toTag: "callee-tag", headers: [`Contact: <${contact}>`]
  })

const flows = doc(
  [
    leg(CALLER_CALL_ID, oneHop(caller, sut), [
      request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1 }),
      ring(CALLER_CALL_ID, sut, caller, 93, "sip:sut@10.0.0.1"),
      ring(CALLER_CALL_ID, sut, caller, 1_089, "sip:sut@10.0.0.1"),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_091, toTag: "callee-tag" }),
      request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_187, toTag: "callee-tag" }),
      request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 9_000, toTag: "callee-tag" }),
      response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 9_001, toTag: "callee-tag" })
    ]),
    leg(CALLEE_CALL_ID, oneHop(sut, callee), [
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 17 }),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 18 }),
      ring(CALLEE_CALL_ID, callee, sut, 92, "sip:9412@10.0.0.2"),
      ring(CALLEE_CALL_ID, callee, sut, 1_060, "sip:9151@10.0.0.2"),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 1_076, toTag: "callee-tag" }),
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_188, toTag: "callee-tag" }),
      request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 9_002, toTag: "callee-tag" }),
      response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 9_010, toTag: "callee-tag" })
    ])
  ],
  [{ legs: [0, 1] }]
)

describe("two provisionals that differ only by Contact", () => {
  it("are two distinct send steps, the target refresh kept", () => {
    const flow = synthesize(flows, build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix), plan())
    const rings = flow.steps.filter((s) => s.leg === "B" && s.op === "send" && s.msg.status === 180)
    expect(rings).toHaveLength(2)
    expect(rings[1]!.msg).not.toEqual(rings[0]!.msg)
  })
})
