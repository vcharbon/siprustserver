/**
 * A response the scripted party OWES on its own transaction — the 200 to a
 * CANCEL (RFC 3261 §9.2), the 487 that CANCEL draws on the INVITE — is
 * anchored on that transaction, never on a later message of the system under
 * test that merely preceded it in the capture. A system that does not send
 * that message must still be answered.
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
const branchOf = (callId: string) => `z9hG4bK-${callId}-1-INVITE`

/**
 * The caller CANCELs and BYEs its early dialog; the platform CANCELs and BYEs
 * the callee, which answers the CANCEL only after the BYE reached it, then the
 * BYE, then the INVITE's 487.
 */
const flows: Flows.FlowsDoc = doc(
  [
    leg(CALLER_CALL_ID, oneHop(caller, sut), [
      request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1 }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 2_941, toTag: "callee-tag" }),
      request({ callId: CALLER_CALL_ID, seq: 1, method: "CANCEL", src: caller, dst: sut, ts_ms: 42_482, branch: branchOf(CALLER_CALL_ID) }),
      request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 42_483, toTag: "callee-tag" }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: sut, dst: caller, ts_ms: 42_484, toTag: "callee-tag", branch: branchOf(CALLER_CALL_ID) }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 42_485, toTag: "callee-tag" }),
      response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 42_486, toTag: "callee-tag" }),
      request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 42_512, toTag: "callee-tag", branch: branchOf(CALLER_CALL_ID) })
    ]),
    leg(CALLEE_CALL_ID, oneHop(sut, callee), [
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 25 }),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 54 }),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 2_940, toTag: "callee-tag" }),
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "CANCEL", src: sut, dst: callee, ts_ms: 42_483.4, branch: branchOf(CALLEE_CALL_ID) }),
      request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 42_501, toTag: "callee-tag" }),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: callee, dst: sut, ts_ms: 42_512.3, toTag: "callee-tag", branch: branchOf(CALLEE_CALL_ID) }),
      response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 42_606, toTag: "callee-tag" }),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 42_616, toTag: "callee-tag" }),
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 42_624, toTag: "callee-tag", branch: branchOf(CALLEE_CALL_ID) })
    ])
  ],
  [{ legs: [0, 1] }]
)

describe("the callee's answers to the platform's CANCEL", () => {
  const flow = synthesize(flows, build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix), plan())
  const on = (op: "send" | "expect", match: (s: (typeof flow.steps)[number]) => boolean) =>
    flow.steps.find((s) => s.leg === "B" && s.op === op && match(s))!
  const cancel = on("expect", (s) => s.msg.method === "CANCEL")
  const bye = on("expect", (s) => s.msg.method === "BYE")
  const cancelOk = on("send", (s) => s.msg.status === 200 && s.msg["cseq-method"] === "CANCEL")
  const terminated = on("send", (s) => s.msg.status === 487)

  it("answers the CANCEL on the CANCEL, not on the BYE that preceded the answer", () => {
    expect(cancelOk.delay.from).toBe(`step:${cancel.id}`)
  })

  /** Every step `step` waits on, following the anchors back to the trigger. */
  const gates = (step: (typeof flow.steps)[number]): Array<string> => {
    const out: Array<string> = []
    let from = step.delay.from
    while (from.startsWith("step:")) {
      out.push(from.slice(5))
      from = flow.steps.find((s) => s.id === from.slice(5))!.delay.from
    }
    return out
  }

  it("gates neither owed answer on the BYE, directly or through another step", () => {
    expect(gates(cancelOk)).not.toContain(bye.id)
    expect(gates(terminated)).not.toContain(bye.id)
  })
})
