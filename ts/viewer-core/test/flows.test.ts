import { describe, expect, it } from "vitest"
import { bodySegments, bodyText } from "../src/flows/body.ts"
import { buildFlowsLayout } from "../src/flows/layout.ts"
import { parseFlowsDocument } from "../src/flows/parse.ts"
import { flowsToText } from "../src/text/flows-text.ts"
import { headBodyDocument, INDATA_DUMP, INFO_DUMP, INVITE_HEAD, SDP } from "./head-body.ts"

const message = (
  tsUs: number,
  src: string,
  dst: string,
  summary: Record<string, unknown>,
  extra: Record<string, unknown> = {}
) => ({ ts_us: tsUs, src, dst, hop: 0, retx: false, raw: "INVITE sip:x SIP/2.0\r\n\r\n", summary, ...extra })

const doc = {
  schema: 5,
  legs: [
    {
      call_id: "call-a",
      final_status: 200,
      terminated_by: "BYE",
      msgs: [
        message(1_000_000, "1.1.1.1:5060", "2.2.2.2:5060", {
          kind: "request",
          method: "INVITE",
          uri: "sip:x@host",
          cseq: { seq: 4, method: "INVITE" }
        }),
        message(1_002_000, "2.2.2.2:5060", "1.1.1.1:5060", {
          kind: "response",
          status: 100,
          reason: "Trying",
          cseq: { seq: 4, method: "INVITE" }
        }, { retx: true })
      ]
    },
    {
      call_id: "call-b",
      msgs: [
        message(1_001_000, "2.2.2.2:5060", "3.3.3.3:5060", {
          kind: "request",
          method: "INVITE",
          cseq: { seq: 1, method: "INVITE" }
        })
      ]
    }
  ],
  groups: [
    { legs: [0], t0_us: 1_000_000, final_status: 200 },
    { legs: [1], t0_us: 1_001_000 }
  ]
}

describe("parseFlowsDocument", () => {
  it("refuses a value that is not a flows document, with a reason", () => {
    expect(parseFlowsDocument(null)).toEqual({ ok: false, reason: "not a JSON object" })
    expect(parseFlowsDocument({ schema: 5 })).toEqual({
      ok: false,
      reason: "no legs[]: not a flows document"
    })
  })

  it("reads the start line, the transaction and the wire bytes of each message", () => {
    const read = parseFlowsDocument(doc)
    if (!read.ok) throw new Error(read.reason)
    const first = read.document.legs[0]?.messages[0]
    expect(first).toMatchObject({
      legIndex: 0,
      msgIndex: 0,
      kind: "request",
      method: "INVITE",
      cseqSeq: 4,
      cseqMethod: "INVITE",
      src: "1.1.1.1:5060",
      callId: "call-a"
    })
    expect(first?.raw.startsWith("INVITE")).toBe(true)
  })

  it("keeps the extractor's retransmission verdict rather than deriving one", () => {
    const read = parseFlowsDocument(doc)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs[0]?.messages[1]?.retx).toBe(true)
  })

  it("orders every leg's messages into one capture timeline", () => {
    const read = parseFlowsDocument(doc)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.messages.map((message) => [message.legIndex, message.msgIndex])).toEqual([
      [0, 0],
      [1, 0],
      [0, 1]
    ])
  })

  it("keeps a group's correlation evidence, which is where derivation is stated", () => {
    const read = parseFlowsDocument({
      ...doc,
      groups: [{
        legs: [0, 1],
        evidence: [
          { kind: "derived_call_id", strategy: 2, legs: [0, 1], prefix: "1-" },
          "nonsense"
        ]
      }]
    })
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.groups[0]?.evidence).toEqual([
      { kind: "derived_call_id", legs: [0, 1], prefix: "1-", suffix: undefined }
    ])
  })

  it("warns about a schema it was not written against instead of failing", () => {
    const read = parseFlowsDocument({ ...doc, schema: 4 })
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.warnings.join(" ")).toContain("flows schema 4")
    expect(read.document.messages).toHaveLength(3)
  })

  it("keeps a message it could not read as a warning, never a silent drop", () => {
    const read = parseFlowsDocument({ ...doc, legs: [{ call_id: "c", msgs: ["nonsense"] }] })
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.warnings.some((warning) => warning.includes("not an object"))).toBe(true)
  })
})

describe("a message whose body is bytes", () => {
  const read = parseFlowsDocument(headBodyDocument)
  if (!read.ok) throw new Error(read.reason)
  const [invite, info] = read.document.messages
  if (invite === undefined || info === undefined) throw new Error("two messages expected")

  it("is read as its head followed by its body, one character per byte", () => {
    expect(invite.binary).toBe(true)
    expect(invite.raw.startsWith(INVITE_HEAD)).toBe(true)
    expect(invite.raw.length).toBe(INVITE_HEAD.length + 333)
    expect(invite.raw.charCodeAt(INVITE_HEAD.length + 291 + 4)).toBe(0x83)
    expect(invite.body?.parts.map((part) => [part.contentType, part.offset, part.len])).toEqual([
      ["application/sdp", 54, 85],
      ["application/vnd.sample.indata", 291, 17]
    ])
  })

  it("counts its bytes as the wire does: head bytes plus body bytes", () => {
    expect(invite.bytes).toBe(653)
    expect(info.bytes).toBe(309)
    const lines = flowsToText(buildFlowsLayout(read.document, { title: "indata" })).split("\n")
    expect(lines.filter((line) => line.startsWith("  m"))).toEqual([
      "  m0 +0.000000 leg0/0 10.0.0.1:5060 -> 10.0.0.2:5060 | INVITE sip:+155500000000900004@example.test | CSeq 1 INVITE | abc/- | 653B",
      "  m1 +0.500000 leg0/1 10.0.0.1:5060 -> 10.0.0.2:5060 | INFO sip:+155500000000900004@example.test | CSeq 2 INFO | abc/xyz | 309B"
    ])
  })

  it("counts a text message's bytes as UTF-8, not as characters", () => {
    const accented = parseFlowsDocument({
      ...doc,
      legs: [{ call_id: "c", msgs: [message(1, "a", "b", {}, { raw: "INVITE sip:é SIP/2.0\r\n\r\n" })] }]
    })
    if (!accented.ok) throw new Error(accented.reason)
    expect(accented.document.messages[0]?.binary).toBe(false)
    expect(accented.document.messages[0]?.bytes).toBe(25)
  })

  it("draws a multipart body part by part: its framing, the SDP as text, the indata as hex", () => {
    const segments = bodySegments(invite)
    expect(segments.map((segment) => segment.kind)).toEqual(["framing", "text", "framing", "hex", "framing"])
    expect(segments[0]?.text).toBe("--unique-boundary-1\r\nContent-Type: application/sdp\r\n\r\n")
    expect(segments[1]?.text).toBe(SDP)
    expect(segments[2]?.text).toBe(
      "\r\n--unique-boundary-1\r\nContent-Type: application/vnd.sample.indata\r\n" +
        "Content-Transfer-Encoding: binary\r\nContent-Disposition: signal;handling=optional\r\n\r\n"
    )
    expect(segments[3]?.text).toBe(INDATA_DUMP)
    expect(segments[4]?.text).toBe("\r\n--unique-boundary-1--\r\n")
  })

  it("draws a body that is one binary part as one hex dump", () => {
    expect(bodySegments(info)).toEqual([{ kind: "hex", text: INFO_DUMP }])
    expect(bodyText(bodySegments(info))).toBe(`${INFO_DUMP}\n`)
  })

  it("draws a text message's body as it always did", () => {
    const read = parseFlowsDocument(doc)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.messages[0]?.binary).toBe(false)
    expect(read.document.messages[0]?.body).toBeUndefined()
    expect(bodySegments(read.document.messages[0]!)).toEqual([])
  })
})

describe("buildFlowsLayout", () => {
  const read = parseFlowsDocument(doc)
  if (!read.ok) throw new Error(read.reason)
  const document = read.document

  it("draws one row per captured message, sockets as lifelines", () => {
    const layout = buildFlowsLayout(document)
    expect(layout.rows).toHaveLength(3)
    expect(layout.lifelines.map((lifeline) => lifeline.id)).toEqual([
      "1.1.1.1:5060",
      "2.2.2.2:5060",
      "3.3.3.3:5060"
    ])
  })

  it("draws each message from its source socket to its destination", () => {
    const layout = buildFlowsLayout(document)
    const first = layout.rows[0]!
    const second = layout.rows[2]!
    expect(first.x1).toBeLessThan(first.x2)
    expect(second.x1).toBeGreaterThan(second.x2)
  })

  it("times every row from the first drawn message", () => {
    const layout = buildFlowsLayout(document)
    expect(layout.rows.map((row) => row.atMs)).toEqual([0, 1, 2])
  })

  it("restricts the drawing to the call groups a case was cut from, on request", () => {
    const layout = buildFlowsLayout(document, { groups: [1], restrictToGroups: true })
    expect(layout.rows).toHaveLength(1)
    expect(layout.rows[0]?.message.callId).toBe("call-b")
  })

  it("draws everything, with a warning, when a case names a group the document lacks", () => {
    const layout = buildFlowsLayout(document, { groups: [7], restrictToGroups: true })
    expect(layout.rows).toHaveLength(3)
    expect(layout.warnings.join(" ")).toContain("call group 7")
  })

  it("labels a response by its status and a request by its method", () => {
    const layout = buildFlowsLayout(document)
    expect(layout.rows.map((row) => row.label)).toEqual(["INVITE", "INVITE", "100 Trying"])
  })
})
