/**
 * How a hunted callee's actor is claimed in the assembled document: by R-URI
 * position where its INVITE names a number, so a lane can bind the number the
 * system dials it on; by arrival order where it names none (a name, or no user
 * part at all), since no number exists for a ruri-pos claim to match.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { assemble } from "../src/assemble.js"
import type { CaseSpec } from "../src/case-spec.js"
import { policyWith } from "../src/policy.js"
import {
  CALLEE_CALL_ID,
  CALLER_CALL_ID,
  CALLER_URI,
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

const { caller, sut, callee } = SOCKETS

const SPEC: CaseSpec = {
  id: "claim-1",
  uac: { leg: 0, hop: 0 },
  uas: [{ leg: 1, hop: 0 }],
  defects: [],
  cutLegs: [0, 1]
}

/** A call the platform relays to `calleeUri`, answered, then ended by the caller. */
const relayedTo = (calleeUri: string): Flows.FlowsDoc =>
  doc(
    [
      leg(CALLER_CALL_ID, oneHop(caller, sut), [
        request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
        response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" }),
        request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 5_000, toTag: "sut-tag" }),
        response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 5_005, toTag: "sut-tag" })
      ]),
      {
        ...leg(CALLEE_CALL_ID, oneHop(sut, callee), [
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10, ruri: calleeUri }),
          response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 990, toTag: "callee-tag", toUri: calleeUri }),
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_010, toTag: "callee-tag", ruri: calleeUri }),
          request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 5_010, toTag: "callee-tag", ruri: calleeUri }),
          response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 5_015, toTag: "callee-tag", toUri: calleeUri })
        ]),
        invite: { ruri: calleeUri, from_uri: CALLER_URI, to_uri: calleeUri, cseq: 1 }
      }
    ],
    [{ legs: [0, 1] }]
  )

/** The claim the assembled document states on the actor answering the call. */
const calleeClaim = (calleeUri: string) => {
  const { pivot } = assemble({
    flows: relayedTo(calleeUri),
    capture: "claim.pcap",
    spec: SPEC,
    sut: sutSet(),
    plan: plan(),
    policy: policyWith({ derives: derivesOnePrefix })
  })
  return pivot.actors.find((a) => a.kind === "uas")!.claim
}

describe("a hunted callee's claim", () => {
  it("is its R-URI position where the INVITE names a number the plan classifies", () => {
    expect(calleeClaim("sip:+15556000004@192.0.2.2")).toEqual({ by: "ruri-pos" })
  })

  it("is its R-URI position where the INVITE names a number the plan does not classify", () => {
    expect(calleeClaim("sip:5550100@192.0.2.2")).toEqual({ by: "ruri-pos" })
  })

  it("is the arrival order where the INVITE names a user that is no number", () => {
    expect(calleeClaim("sip:bob@biloxi.example.com")).toEqual({ by: "arrival-order" })
  })

  it("is the arrival order where the INVITE's R-URI has no user part", () => {
    expect(calleeClaim("sip:192.0.2.2:5060")).toEqual({ by: "arrival-order" })
  })
})
