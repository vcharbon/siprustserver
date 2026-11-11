/**
 * A recorded datagram's head facts: a request's method, a response's status,
 * the CSeq method and the Call-ID, in each arm a line carries its datagram in,
 * a compact header name read as its full name.
 */
import { describe, expect, it } from "vitest"
import { decodeRecordedMessageSync, recordedHeadOf } from "../src/bundle/index.js"

const line = (fields: Record<string, unknown>) => decodeRecordedMessageSync({ seq: 1, dir: "out", at_us: 0, ...fields })

describe("the head facts of a recorded datagram", () => {
  it("reads a request's method, CSeq method and a compact Call-ID", () => {
    const raw = "invite sip:b@example.invalid SIP/2.0\r\ni: c1@example.invalid\r\nCSeq: 1 invite\r\n\r\n"
    expect(recordedHeadOf(line({ raw }))).toEqual({
      method: "INVITE",
      status: undefined,
      cseqMethod: "INVITE",
      callId: "c1@example.invalid"
    })
  })

  it("reads a response's status from the head arm, its body never read as a header", () => {
    const head = "SIP/2.0 200 OK\r\nCall-ID: c1@example.invalid\r\nCSeq: 1 INVITE\r\nContent-Length: 4\r\n\r\n"
    const message = line({ head, body_b64: btoa("ÿþA:") })
    expect(recordedHeadOf(message)).toEqual({ method: undefined, status: 200, cseqMethod: "INVITE", callId: "c1@example.invalid" })
  })

  it("states nothing a head does not carry", () => {
    expect(recordedHeadOf(line({ raw: "garbage\r\n\r\n" }))).toEqual({
      method: undefined,
      status: undefined,
      cseqMethod: undefined,
      callId: undefined
    })
  })
})
