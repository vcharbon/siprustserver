/**
 * The expect side of the body registry: a frozen text body is stored and
 * asserted by content, an SDP stored and compared as a session description,
 * multipart by shape, absence as its own claim, and a binary payload stays
 * undeclared. The send side is covered where the flow is synthesized
 * (`flowsteps.test.ts`).
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { decompose, expectBody } from "../src/bodies.js"
import { synthesize } from "../src/flowsteps.js"
import { index } from "../src/parts.js"
import type { Vantage } from "../src/selection.js"
import { build } from "../src/topology.js"
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

const XML = '<?xml version="1.0" encoding="utf-8"?>\r\n<request><play><prompt><audio url="a.wav"/></prompt></play></request>\r\n'
const example = { contentType: "application/example+xml", text: XML }
const { caller, callee, sut } = SOCKETS

const info = (o: { readonly text?: string; readonly contentType?: string } = {}): Flows.Msg =>
  request({
    callId: CALLER_CALL_ID,
    seq: 2,
    method: "INFO",
    src: sut,
    dst: caller,
    ts_ms: 1_500,
    toTag: "sut-tag",
    body: { contentType: o.contentType ?? example.contentType, text: o.text ?? example.text }
  })

describe("expectBody", () => {
  it("stores a frozen text body as a resource and asserts it by content", () => {
    const stored = expectBody(info(), "uac1_r3")
    expect(stored.body).toEqual({
      ref: "resources/uac1_r3_0.xml",
      mode: "frozen",
      "content-type": "application/example+xml"
    })
    expect(stored.resources).toEqual([{ relPath: "resources/uac1_r3_0.xml", bytes: new TextEncoder().encode(XML) }])
    expect(stored.flags).toEqual([])
  })

  it("keeps the captured content type verbatim, parameters included", () => {
    const stored = expectBody(info({ contentType: "application/example+xml;charset=utf-8" }), "uac1_r0")
    expect(stored.body).toMatchObject({ "content-type": "application/example+xml;charset=utf-8" })
  })

  it("flags an unrecognized text body carrying number-like digits, as the send side does", () => {
    const stored = expectBody(info({ contentType: "application/vnd.example", text: "id=0033612345678" }), "uac1_r0")
    expect(stored.body).toMatchObject({ ref: "resources/uac1_r0_0.bin", mode: "frozen" })
    expect(stored.flags.map((f) => f.kind)).toEqual(["unrecognized-body-part"])
  })

  it("stores an SDP as a resource compared as a session description, under the registry's rewrite tokens", () => {
    const sdp = response({
      callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE",
      src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag", sdp: "v=0\r\n"
    })
    expect(expectBody(sdp, "uac1_r0")).toEqual({
      body: { ref: "resources/uac1_r0_0.sdp", rewrite: ["c=addr", "m=port"], compare: "sdp" },
      resources: [{ relPath: "resources/uac1_r0_0.sdp", bytes: new TextEncoder().encode("v=0\r\n") }],
      flags: []
    })
    // A parameterised type is stated, as on the send side; bare `application/sdp` is derived.
    const typed = expectBody(info({ contentType: "application/sdp;charset=utf-8", text: "v=0\r\n" }), "uac1_r0")
    expect(typed.body).toMatchObject({ "content-type": "application/sdp;charset=utf-8", compare: "sdp" })
  })

  it("asserts multipart and absence by shape, with no resource", () => {
    const mixed = info({ contentType: 'multipart/mixed;boundary="b"', text: "--b\r\n--b--\r\n" })
    expect(expectBody(mixed, "uac1_r0")).toEqual({ body: { mode: "multipart-present" }, resources: [], flags: [] })
    const bare = request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" })
    expect(expectBody(bare, "uac1_r0")).toEqual({ body: { mode: "absent" }, resources: [], flags: [] })
  })

  /** The message as extraction hands a binary payload over: `head` + `body_b64`. */
  const asBinary = (text: Flows.Msg): Flows.Msg => {
    const { raw, ...rest } = text as Flows.Msg & { raw: string }
    const cut = raw.indexOf("\r\n\r\n")
    return {
      ...rest,
      head: raw.slice(0, cut),
      body_b64: Buffer.from(raw.slice(cut + 4), "latin1").toString("base64")
    } as Flows.Msg
  }

  it("stores an SDP handed over split as a resource carrying the bytes it came as, as the send side does", () => {
    const stored = expectBody(asBinary(info({ contentType: "application/sdp", text: "v=0\r\n" })), "uac1_r0")
    expect(stored.body).toMatchObject({ ref: "resources/uac1_r0_0.sdp", compare: "sdp" })
    expect(stored.resources).toEqual([{ relPath: "resources/uac1_r0_0.sdp", bytes: new TextEncoder().encode("v=0\r\n") }])
  })
})

describe("an expected body in a synthesized flow", () => {
  const BOTH: ReadonlyArray<Vantage> = [
    { leg: 0, hop: 0 },
    { leg: 1, hop: 0 }
  ]

  /** One INFO relayed across the platform: sent on the caller's leg, expected on the callee's. */
  const relayedInfoFlows = (): Flows.FlowsDoc =>
    doc(
      [
        leg(CALLER_CALL_ID, oneHop(caller, sut), [
          request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0 }),
          response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag" }),
          request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" }),
          request({ callId: CALLER_CALL_ID, seq: 2, method: "INFO", src: caller, dst: sut, ts_ms: 1_500, toTag: "sut-tag", body: example }),
          response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INFO", src: sut, dst: caller, ts_ms: 1_530, toTag: "sut-tag" }),
          request({ callId: CALLER_CALL_ID, seq: 3, method: "BYE", src: caller, dst: sut, ts_ms: 2_000, toTag: "sut-tag" }),
          response({ callId: CALLER_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 2_005, toTag: "sut-tag" })
        ]),
        leg(CALLEE_CALL_ID, oneHop(sut, callee), [
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10 }),
          response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 990, toTag: "callee-tag" }),
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_010, toTag: "callee-tag" }),
          request({ callId: CALLEE_CALL_ID, seq: 2, method: "INFO", src: sut, dst: callee, ts_ms: 1_510, toTag: "callee-tag", body: example }),
          response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "INFO", src: callee, dst: sut, ts_ms: 1_520, toTag: "callee-tag" }),
          request({ callId: CALLEE_CALL_ID, seq: 3, method: "BYE", src: sut, dst: callee, ts_ms: 2_010, toTag: "callee-tag" }),
          response({ callId: CALLEE_CALL_ID, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 2_015, toTag: "callee-tag" })
        ])
      ],
      [{ legs: [0, 1] }]
    )

  it("is stored beside the send of the same message under a name of its own", () => {
    const flows = relayedInfoFlows()
    const flow = synthesize(flows, build(flows, BOTH, sutSet(), plan(), derivesOnePrefix), plan())
    const infos = flow.steps.filter((s) => (s.msg.method ?? "").toUpperCase() === "INFO")
    const sent = infos.find((s) => s.op === "send")!
    const expected = infos.find((s) => s.op === "expect")!
    const refOf = (s: typeof sent) => (s.msg.body as { ref: string }).ref
    expect(refOf(sent)).toMatch(/^resources\/[a-z0-9]+_\d+_0\.xml$/)
    expect(refOf(expected)).toMatch(/^resources\/[a-z0-9]+_r\d+_0\.xml$/)
    expect(expected.msg.body).toEqual({ ref: refOf(expected), mode: "frozen", "content-type": "application/example+xml" })
    expect(refOf(sent)).not.toBe(refOf(expected))
    const byPath = new Map(flow.resources.map((r) => [r.relPath, new TextDecoder().decode(r.bytes)]))
    expect(byPath.get(refOf(sent))).toBe(XML)
    expect(byPath.get(refOf(expected))).toBe(XML)
    expect(flow.resources.map((r) => r.relPath)).toHaveLength(new Set(flow.resources.map((r) => r.relPath)).size)
  })

  it("an SDP relayed on the INVITE is stored on the send and on the expect under two names", () => {
    const offer = "v=0\r\no=- 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 6000 RTP/AVP 8\r\n"
    const flows = doc(
      [
        leg(CALLER_CALL_ID, oneHop(caller, sut), [
          request({ callId: CALLER_CALL_ID, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0, body: { contentType: "application/sdp", text: offer } }),
          response({ callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag" }),
          request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" }),
          request({ callId: CALLER_CALL_ID, seq: 2, method: "BYE", src: caller, dst: sut, ts_ms: 2_000, toTag: "sut-tag" }),
          response({ callId: CALLER_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 2_005, toTag: "sut-tag" })
        ]),
        leg(CALLEE_CALL_ID, oneHop(sut, callee), [
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "INVITE", src: sut, dst: callee, ts_ms: 10, body: { contentType: "application/sdp", text: offer } }),
          response({ callId: CALLEE_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: callee, dst: sut, ts_ms: 990, toTag: "callee-tag" }),
          request({ callId: CALLEE_CALL_ID, seq: 1, method: "ACK", src: sut, dst: callee, ts_ms: 1_010, toTag: "callee-tag" }),
          request({ callId: CALLEE_CALL_ID, seq: 2, method: "BYE", src: sut, dst: callee, ts_ms: 2_010, toTag: "callee-tag" }),
          response({ callId: CALLEE_CALL_ID, seq: 2, status: 200, reason: "OK", cseqMethod: "BYE", src: callee, dst: sut, ts_ms: 2_015, toTag: "callee-tag" })
        ])
      ],
      [{ legs: [0, 1] }]
    )
    const flow = synthesize(flows, build(flows, BOTH, sutSet(), plan(), derivesOnePrefix), plan())
    const invites = flow.steps.filter((s) => (s.msg.method ?? "").toUpperCase() === "INVITE")
    const sent = invites.find((s) => s.op === "send")!
    const expected = invites.find((s) => s.op === "expect")!
    const refOf = (s: typeof sent) => (s.msg.body as { ref: string }).ref
    expect(sent.msg.body).toEqual({ ref: refOf(sent), rewrite: ["c=addr", "m=port"] })
    expect(refOf(sent)).toMatch(/^resources\/[a-z0-9]+_\d+_0\.sdp$/)
    expect(expected.msg.body).toEqual({ ref: refOf(expected), rewrite: ["c=addr", "m=port"], compare: "sdp" })
    expect(refOf(expected)).toMatch(/^resources\/[a-z0-9]+_r\d+_0\.sdp$/)
    expect(refOf(sent)).not.toBe(refOf(expected))
    const byPath = new Map(flow.resources.map((r) => [r.relPath, new TextDecoder().decode(r.bytes)]))
    expect(byPath.get(refOf(sent))).toBe(offer)
    expect(byPath.get(refOf(expected))).toBe(offer)
  })
})

/**
 * Bytes are the truth of a body. An expect states a single body as a frozen
 * resource whatever its bytes hold — text or not — and a multipart reception
 * part by part, each part a resource compared under its own mode; the resource
 * files carry bytes. No content type says binary: only the bytes do.
 */
describe("expectBody over bytes", () => {
  const BLOB = Uint8Array.from([0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x00])
  const BLOB_LATIN1 = String.fromCharCode(...BLOB)
  const BLOB_TYPE = "application/vnd.example.blob"
  const SDP = "v=0\r\no=- 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 6000 RTP/AVP 8\r\n"
  const utf8 = new TextEncoder()

  /** The message as extraction hands a payload that is not UTF-8 over: `head` + `body_b64`, the body's bytes latin1 in `text`. */
  const asBinary = (m: Flows.Msg): Flows.Msg => {
    const { raw, ...rest } = m as Flows.Msg & { raw: string }
    const cut = raw.indexOf("\r\n\r\n")
    return {
      ...rest,
      head: raw.slice(0, cut + 4),
      body_b64: Buffer.from(raw.slice(cut + 4), "latin1").toString("base64")
    } as Flows.Msg
  }

  it("states a single binary body as a frozen resource carrying its bytes", () => {
    const stored = expectBody(asBinary(info({ contentType: BLOB_TYPE, text: BLOB_LATIN1 })), "uac1_r0")
    expect(stored.body).toEqual({ ref: "resources/uac1_r0_0.bin", mode: "frozen", "content-type": BLOB_TYPE })
    expect(stored.resources).toHaveLength(1)
    expect(stored.resources[0]?.relPath).toBe("resources/uac1_r0_0.bin")
    expect(stored.resources[0]?.bytes).toEqual(BLOB)
    expect(stored.flags).toEqual([])
  })

  it("states a text body's resource as bytes too: one resource shape", () => {
    const stored = expectBody(info(), "uac1_r3")
    expect(stored.body).toEqual({ ref: "resources/uac1_r3_0.xml", mode: "frozen", "content-type": "application/example+xml" })
    expect(stored.resources[0]?.bytes).toEqual(utf8.encode(XML))
    expect(stored.resources[0]).not.toHaveProperty("text")
    expect(stored.resources[0]).not.toHaveProperty("binary")
  })

  it("handles an emergency-call-data member type like the rest of its family: frozen, no binary mode", () => {
    // RFC 8147's `application/EmergencyCallData.eCall.MSD` is one member of the
    // RFC 7852 family; the registry freezes the family by prefix, and the
    // bytes alone say whether the payload is text.
    const binary = asBinary(info({ contentType: "application/EmergencyCallData.eCall.MSD", text: BLOB_LATIN1 }))
    const stored = expectBody(binary, "uac1_r0")
    expect(stored.body).toEqual({
      ref: "resources/uac1_r0_0.bin",
      mode: "frozen",
      "content-type": "application/EmergencyCallData.eCall.MSD"
    })
    expect(stored.resources[0]?.bytes).toEqual(BLOB)
    const sent = decompose(binary, "uac1_0")
    expect(sent.body).toMatchObject({ mode: "frozen" })
    expect(JSON.stringify(sent.body)).not.toContain("frozen-binary")
  })

  describe("a multipart reception", () => {
    const framing1 = "--b1\r\nContent-Type: application/sdp\r\nContent-ID: <offer@example.invalid>\r\n\r\n"
    const framing2 = "\r\n--b1\r\nContent-Type: application/vnd.example.blob\r\nContent-Transfer-Encoding: binary\r\n\r\n"
    const closing = "\r\n--b1--\r\n"
    const body = framing1 + SDP + framing2 + BLOB_LATIN1 + closing
    const sdpAt = framing1.length
    const blobAt = framing1.length + SDP.length + framing2.length

    const mixed = (): Flows.Msg => {
      const m = asBinary(info({ contentType: "multipart/mixed;boundary=b1", text: body }))
      return {
        ...m,
        body: {
          content_type: "multipart/mixed",
          len: body.length,
          parts: [
            { content_type: "application/sdp", content_id: "<offer@example.invalid>", offset: sdpAt, len: SDP.length },
            {
              content_type: "application/vnd.example.blob",
              headers: [{ name: "Content-Transfer-Encoding", value: "binary" }],
              offset: blobAt,
              len: BLOB.length
            }
          ]
        }
      } as Flows.Msg
    }

    it("is stated part by part: the SDP part compared as a session description, the binary part frozen", () => {
      const flows = doc([leg(CALLER_CALL_ID, oneHop(caller, sut), [mixed()])], [{ legs: [0] }])
      const m = flows.legs[0]!.msgs[0]!
      const parts = index(flows).get(m)
      expect(parts?.parts).toHaveLength(2)
      const stored = expectBody(m, "uac1_r0", parts)
      expect(stored.body).toEqual({
        multipart: {
          "content-type": "multipart/mixed",
          parts: [
            {
              "content-type": "application/sdp",
              ref: "resources/uac1_r0_0.sdp",
              rewrite: ["c=addr", "m=port"],
              compare: "sdp",
              "content-id": "<offer@example.invalid>"
            },
            {
              "content-type": "application/vnd.example.blob",
              ref: "resources/uac1_r0_1.bin",
              mode: "frozen",
              headers: [{ name: "Content-Transfer-Encoding", value: "binary" }]
            }
          ]
        }
      })
      expect(stored.resources.map((r) => r.relPath)).toEqual(["resources/uac1_r0_0.sdp", "resources/uac1_r0_1.bin"])
      expect(stored.resources[0]?.bytes).toEqual(utf8.encode(SDP))
      expect(stored.resources[1]?.bytes).toEqual(BLOB)
      expect(stored.flags).toEqual([])
    })

    it("falls back to the shape where extraction handed no parts over", () => {
      expect(expectBody(mixed(), "uac1_r0")).toMatchObject({ body: { mode: "multipart-present" }, resources: [] })
    })
  })
})

/**
 * THE body on the cut side is the one the confronter reads: the extractor's
 * layout bounds it (the parser's `Content-Length`), and bytes the datagram
 * carried past that length are on the wire, never in a resource.
 */
describe("the body the cut stores is layout-bounded", () => {
  const BLOB = Uint8Array.from([0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x00])
  const BLOB_TYPE = "application/vnd.example.blob"
  const utf8 = new TextEncoder()

  /** A head-body message whose tail runs `trailing` bytes past its declared length. */
  const trailing = (body: Uint8Array, contentType: string, declared: number, layout: boolean, trailing: string): Flows.Msg => {
    const m = info({ contentType, text: "" }) as Flows.Msg & { raw: string }
    const { raw, body: _layout, ...rest } = m as Flows.Msg & { raw: string; body?: unknown }
    const head = raw.replace(/Content-Length: \d+/, `Content-Length: ${declared}`)
    const tail = new Uint8Array(body.length + trailing.length)
    tail.set(body, 0)
    tail.set(utf8.encode(trailing), body.length)
    return {
      ...rest,
      head,
      body_b64: Buffer.from(tail).toString("base64"),
      ...(layout ? { body: { content_type: contentType, len: declared } } : {})
    } as Flows.Msg
  }

  it("stores the declared bytes of a tail that runs past them, on the expect and the send side alike", () => {
    const m = trailing(BLOB, BLOB_TYPE, BLOB.length, true, "\r\n")
    expect(expectBody(m, "uac1_r0").resources[0]?.bytes).toEqual(BLOB)
    expect(decompose(m, "uac1_0").resources[0]?.bytes).toEqual(BLOB)
  })

  it("stores no body where no layout is stated, whatever the head declares or the tail carries", () => {
    // No Content-Length at all: the parser read no body, so it wrote no layout,
    // and the layout is the one rule — not the header, not the tail.
    const m = trailing(BLOB, BLOB_TYPE, 0, false, "")
    const { head, ...rest } = m as Flows.Msg & { head: string }
    const unlengthed = { ...rest, head: head.replace(/Content-Length: \d+\r\n/, "") } as Flows.Msg
    expect(unlengthed).not.toHaveProperty("body")
    expect(expectBody(unlengthed, "uac1_r0")).toEqual({ body: { mode: "absent" }, resources: [], flags: [] })
    expect(decompose(unlengthed, "uac1_0")).toEqual({ body: undefined, resources: [], flags: [], undecomposed: false })
  })

  it("stores no body where the head declares none, whatever the tail carries and with no layout stated", () => {
    const m = trailing(BLOB, BLOB_TYPE, 0, false, "")
    expect(expectBody(m, "uac1_r0")).toEqual({ body: { mode: "absent" }, resources: [], flags: [] })
    expect(decompose(m, "uac1_0")).toEqual({ body: undefined, resources: [], flags: [], undecomposed: false })
  })
})
