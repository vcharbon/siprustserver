import { Bundle } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { receptionsOf } from "../src/receptions.js"

const line = (raw: string, over: Record<string, unknown> = {}): Bundle.RecordedMessage =>
  Bundle.decodeRecordedMessageSync({ at_us: 0, dir: "in", seq: 1, raw, ...over })

const crlf = (lines: ReadonlyArray<string>): string => [...lines, ""].join("\r\n")

const notify = (state: string, frag: string, step?: string, repeat = false) => {
  const body = `${frag}\r\n`
  return line(
    crlf([
      "NOTIFY sip:t@192.0.2.2 SIP/2.0",
      "CSeq: 3 NOTIFY",
      "Event: refer",
      `Subscription-State: ${state}`,
      "Content-Type: message/sipfrag",
      `Content-Length: ${body.length}`,
      ""
    ]) + body,
    {
      body: { content_type: "message/sipfrag", len: body.length },
      ...(step === undefined ? {} : { step }),
      ...(repeat ? { repeat_of: 1 } : {})
    }
  )
}

describe("receptionsOf", () => {
  it("states each received datagram's step, start line, CSeq method, subscription state, sipfrag status and origin", () => {
    const sdp = "v=0\r\no=- 7 8 IN IP4 192.0.2.1\r\ns=-\r\n"
    const ack = line(
      crlf(["ACK sip:t@192.0.2.2 SIP/2.0", "CSeq: 1 ACK", "Content-Type: application/sdp", `Content-Length: ${sdp.length}`, ""]) + sdp,
      { body: { content_type: "application/sdp", len: sdp.length }, step: "s9" }
    )
    const answer = line(crlf(["SIP/2.0 200 OK", "CSeq: 1 CANCEL", "Content-Length: 0", ""]))
    const sent = line(crlf(["BYE sip:t@192.0.2.2 SIP/2.0", "CSeq: 4 BYE", "Content-Length: 0", ""]), { dir: "out" })
    const got = receptionsOf(
      new Map([
        ["A", [ack, answer, sent, notify("pending;expires=60", "SIP/2.0 100 Trying", "s11"), notify("terminated;reason=noresource", "SIP/2.0 403 Forbidden")]],
        ["B", [sent]]
      ])
    )
    expect(got.get("A")).toEqual([
      { step: "s9", method: "ACK", cseqMethod: "ACK", origin: "o=- 7 8 IN IP4 192.0.2.1" },
      { status: 200, cseqMethod: "CANCEL" },
      { step: "s11", method: "NOTIFY", cseqMethod: "NOTIFY", subscriptionState: "pending", fragStatus: 100 },
      { method: "NOTIFY", cseqMethod: "NOTIFY", subscriptionState: "terminated", fragStatus: 403 }
    ])
    // A leg that only sent has no entry.
    expect(got.has("B")).toBe(false)
  })

  it("leaves retransmissions out", () => {
    const got = receptionsOf(new Map([["A", [notify("active", "SIP/2.0 100 Trying", "s11"), notify("active", "SIP/2.0 100 Trying", undefined, true)]]]))
    expect(got.get("A")).toHaveLength(1)
  })
})
