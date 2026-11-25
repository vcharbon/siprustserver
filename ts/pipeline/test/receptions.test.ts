import { Bundle } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { byeAheadOfInviteFinal, receptionsOf, sentOf } from "../src/receptions.js"

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
  it("states each received datagram's step, start line, CSeq number and method, subscription state, sipfrag status and origin", () => {
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
      { step: "s9", method: "ACK", cseqMethod: "ACK", cseq: 1, origin: "o=- 7 8 IN IP4 192.0.2.1" },
      { status: 200, cseqMethod: "CANCEL", cseq: 1 },
      { step: "s11", method: "NOTIFY", cseqMethod: "NOTIFY", cseq: 3, subscriptionState: "pending", fragStatus: 100 },
      { method: "NOTIFY", cseqMethod: "NOTIFY", cseq: 3, subscriptionState: "terminated", fragStatus: 403 }
    ])
    // A leg that only sent has no entry.
    expect(got.has("B")).toBe(false)
  })

  it("leaves retransmissions out", () => {
    const got = receptionsOf(new Map([["A", [notify("active", "SIP/2.0 100 Trying", "s11"), notify("active", "SIP/2.0 100 Trying", undefined, true)]]]))
    expect(got.get("A")).toHaveLength(1)
  })
})

describe("sentOf", () => {
  it("states each datagram the leg's actor sent, its From included, retransmissions left out", () => {
    const update = line(crlf(["UPDATE sip:t@192.0.2.2 SIP/2.0", "From: <sip:+339@h.example>;tag=x", "CSeq: 5 UPDATE", "Content-Length: 0", ""]), { dir: "out", step: "s20" })
    const again = line(crlf(["UPDATE sip:t@192.0.2.2 SIP/2.0", "From: <sip:+339@h.example>;tag=x", "CSeq: 5 UPDATE", "Content-Length: 0", ""]), { dir: "out", repeat_of: 1 })
    const received = line(crlf(["SIP/2.0 200 OK", "From: <sip:+339@h.example>;tag=x", "CSeq: 5 UPDATE", "Content-Length: 0", ""]))
    const got = sentOf(new Map([["A", [update, again, received]], ["B", [received]]]))
    expect(got.get("A")).toEqual([{ step: "s20", method: "UPDATE", cseqMethod: "UPDATE", cseq: 5, from: "<sip:+339@h.example>;tag=x" }])
    expect(got.has("B")).toBe(false)
  })
})

describe("byeAheadOfInviteFinal", () => {
  const out = (lines: ReadonlyArray<string>) => line(crlf([...lines, "Content-Length: 0", ""]), { dir: "out" })
  const into = (lines: ReadonlyArray<string>) => line(crlf([...lines, "Content-Length: 0", ""]))
  const invite = out(["INVITE sip:t@192.0.2.2 SIP/2.0", "CSeq: 1 INVITE"])
  const bye = out(["BYE sip:t@192.0.2.2 SIP/2.0", "CSeq: 2 BYE"])
  const ringing = into(["SIP/2.0 180 Ringing", "CSeq: 1 INVITE"])
  const terminated = into(["SIP/2.0 487 Request Terminated", "CSeq: 1 INVITE"])

  it("names the leg whose BYE left before any final to its INVITE arrived, a provisional being no final", () => {
    const got = byeAheadOfInviteFinal(new Map([["A", [invite, ringing, bye, terminated]], ["C", [invite, bye]]]))
    expect([...got].sort()).toEqual(["A", "C"])
  })

  it("leaves out the leg whose final came first, and a leg that sent no INVITE", () => {
    const got = byeAheadOfInviteFinal(new Map([["B", [invite, terminated, bye]], ["D", [into(["INVITE sip:a@192.0.2.1 SIP/2.0", "CSeq: 1 INVITE"]), bye]]]))
    expect(got.size).toBe(0)
  })
})

describe("a summary's Record-Route", () => {
  it("states every Record-Route value the datagram carries, in order, and none where it carries none", () => {
    const answer = line(
      crlf(["SIP/2.0 200 OK", "Record-Route: <sip:192.0.2.7;lr>, <sip:192.0.2.8;lr>", "Record-Route: <sip:192.0.2.9;lr>", "CSeq: 1 INVITE", "Content-Length: 0", ""]),
      { dir: "out" }
    )
    const plain = line(crlf(["SIP/2.0 200 OK", "CSeq: 2 BYE", "Content-Length: 0", ""]), { dir: "out" })
    expect(sentOf(new Map([["B", [answer, plain]]])).get("B")).toEqual([
      { status: 200, cseqMethod: "INVITE", cseq: 1, recordRoute: ["<sip:192.0.2.7;lr>", "<sip:192.0.2.8;lr>", "<sip:192.0.2.9;lr>"] },
      { status: 200, cseqMethod: "BYE", cseq: 2 }
    ])
  })
})
