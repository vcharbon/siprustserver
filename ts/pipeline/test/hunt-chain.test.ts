/**
 * A serial hunt whose next attempt is dialled between the platform's CANCEL
 * of the previous one and that attempt's 487. The CANCEL is where the platform
 * gave the attempt up (RFC 3261 §9.1); the 487 that settles it only confirms
 * it, and may cross the next INVITE on the wire. The next attempt is the
 * chain's next position, never a parallel branch.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { build } from "../src/topology.js"
import type { Vantage } from "../src/selection.js"
import { CALLER_CALL_ID, doc, leg, oneHop, plan, request, response, SOCKETS, sutSet } from "./fixtures.js"

const { caller, callee, other, sut } = SOCKETS
const FIRST = `1-${CALLER_CALL_ID}`
const SECOND = `2-${CALLER_CALL_ID}`
const FIRST_URI = "sip:+15556000004@10.0.0.2"
const SECOND_URI = "sip:+15556000005@10.0.0.4"

const flows: Flows.FlowsDoc = doc(
  [
    leg(CALLER_CALL_ID, oneHop(caller, sut), [
      request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 9 }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_704, toTag: "sut-tag" }),
      response({ callId: CALLER_CALL_ID, seq: 1, status: 480, reason: "Temporarily Unavailable", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 29_577, toTag: "sut-tag" }),
      request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 29_578, toTag: "sut-tag" })
    ]),
    leg(FIRST, oneHop(sut, callee), [
      request({ callId: FIRST, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 36, ruri: FIRST_URI }),
      response({ callId: FIRST, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 37, toUri: FIRST_URI }),
      response({ callId: FIRST, seq: 1, status: 180, reason: "Ringing", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 3_007, toTag: "first-tag", toUri: FIRST_URI }),
      request({ callId: FIRST, seq: 1, method: "CANCEL", src: sut, dst: callee, ts_ms: 21_711, ruri: FIRST_URI, branch: `z9hG4bK-${FIRST}-1-INVITE` }),
      response({ callId: FIRST, seq: 1, status: 200, reason: "OK", cseqMethod: "CANCEL", src: callee, dst: sut, ts_ms: 21_712, toTag: "first-tag", toUri: FIRST_URI, branch: `z9hG4bK-${FIRST}-1-INVITE` }),
      response({ callId: FIRST, seq: 1, status: 487, reason: "Request Terminated", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 21_725, toTag: "first-tag", toUri: FIRST_URI }),
      request({ callId: FIRST, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 21_734, toTag: "first-tag", ruri: FIRST_URI, branch: `z9hG4bK-${FIRST}-1-INVITE` })
    ]),
    leg(SECOND, oneHop(sut, other), [
      request({ callId: SECOND, seq: 1, method: "INVITE", src: sut, dst: other, ts_ms: 21_724, ruri: SECOND_URI }),
      response({ callId: SECOND, seq: 1, status: 100, reason: "Trying", cseqMethod: "INVITE", src: other, dst: sut, ts_ms: 21_725, toUri: SECOND_URI }),
      response({ callId: SECOND, seq: 1, status: 480, reason: "Temporarily Unavailable", cseqMethod: "INVITE", src: other, dst: sut, ts_ms: 29_562, toTag: "second-tag", toUri: SECOND_URI }),
      request({ callId: SECOND, seq: 1, method: "ACK", src: sut, dst: other, ts_ms: 29_571, toTag: "second-tag", ruri: SECOND_URI })
    ])
  ],
  [{ legs: [0, 1, 2] }]
)

const VANTAGES: ReadonlyArray<Vantage> = [{ leg: 0, hop: 0 }, { leg: 1, hop: 0 }, { leg: 2, hop: 0 }]

describe("a reroute dialled before the cancelled attempt's 487 arrives", () => {
  it("is the chain's next position on branch 0, not a second branch", () => {
    const layout = build(flows, VANTAGES, sutSet(), plan(), (base, derived) => derived.endsWith(`-${base}`))
    const posOf = (origLeg: number) => layout.actorsObs.find((a) => a.origLeg === origLeg)?.pos
    expect([posOf(1), posOf(2)]).toEqual(["called[0][0]", "called[0][1]"])
  })
})
