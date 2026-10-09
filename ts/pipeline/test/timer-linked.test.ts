/**
 * A delay is timer-linked only where a timer sent the message — a session
 * refresh, an expiry BYE, a no-answer CANCEL (RFC 4028 §10, RFC 3261 §13.3.1.1).
 * A message that merely CARRIES `Session-Expires` is not: a 2xx stating the
 * session interval it accepts went out when the callee answered, and its dwell
 * is the callee's latency, which a replay compresses like any other.
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
const TIMER = ["Session-Expires: 1800;refresher=uac", "Require: timer"]

/** The callee rings at 1 404 ms and answers 1 016 ms later, its 2xx stating a session interval. */
const flows = doc(
  [
    leg(CALLER_CALL_ID, oneHop(caller, sut), [
      request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1 }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_405, toTag: "callee-tag" }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 2_421, toTag: "callee-tag", headers: TIMER, sdp: ANSWER_SDP }),
      request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 2_435, toTag: "callee-tag" }),
      request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 9_000, toTag: "callee-tag" }),
      response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 9_001, toTag: "callee-tag" })
    ]),
    leg(CALLEE_CALL_ID, oneHop(sut, callee), [
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 70 }),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 71 }),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 1_404, toTag: "callee-tag" }),
      response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 2_420, toTag: "callee-tag", headers: TIMER, sdp: ANSWER_SDP }),
      request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 2_437, toTag: "callee-tag" }),
      request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 9_002, toTag: "callee-tag" }),
      response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 9_010, toTag: "callee-tag" })
    ])
  ],
  [{ legs: [0, 1] }]
)

describe("a 2xx that carries Session-Expires", () => {
  it("is not timer-linked on either leg: the callee answered, no timer fired", () => {
    const flow = synthesize(flows, build(flows, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix), plan())
    const answers = flow.steps.filter((s) => s.msg.status === 200 && s.msg["cseq-method"] === "INVITE")
    expect(answers).toHaveLength(2)
    expect(answers.map((s) => [s.leg, s.op, s.delay.timer_linked])).toEqual([
      ["B", "send", false],
      ["A", "expect", false]
    ])
  })
})

describe("an in-dialog re-INVITE that refreshes the session", () => {
  /** The callee refreshes the session 900 s after the answer (RFC 4028 §10). */
  const refreshed = doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 2_421, toTag: "callee-tag", headers: TIMER, sdp: ANSWER_SDP }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 2_435, toTag: "callee-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 1_000_000, toTag: "callee-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 1_000_001, toTag: "callee-tag" })
      ]),
      leg(CALLEE_CALL_ID, oneHop(sut, callee), [
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 70 }),
        response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 2_420, toTag: "callee-tag", headers: TIMER, sdp: ANSWER_SDP }),
        request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 2_437, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "INVITE", src: callee, dst: sut, ts_ms: 902_437, toTag: "callee-tag", headers: ["Session-Expires: 1800;refresher=uas"], body: { contentType: "application/sdp", text: ANSWER_SDP } }),
        response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: callee, ts_ms: 902_440, toTag: "callee-tag", sdp: ANSWER_SDP }),
        request({ callId: CALLEE_CALL_ID, seq: 2, method: "ACK", src: callee, dst: sut, ts_ms: 902_450, toTag: "callee-tag" }),
        request({ callId: CALLEE_CALL_ID, seq: 3, method: "BYE", src: sut, dst: callee, ts_ms: 1_000_002, toTag: "callee-tag" }),
        response({ callId: CALLEE_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 1_000_010, toTag: "callee-tag" })
      ])
    ],
    [{ legs: [0, 1] }]
  )

  it("is timer-linked: the party's session timer sent it", () => {
    const flow = synthesize(refreshed, build(refreshed, BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix), plan())
    const refresh = flow.steps.find((s) => s.leg === "B" && s.op === "send" && s.msg.method === "INVITE")
    expect(refresh?.delay.timer_linked).toBe(true)
  })
})
