/**
 * The body confrontation over BYTES: a frozen body compares byte for byte with
 * the resource, whatever the bytes hold; `sdp` and `xml` fold both sides after
 * a strict UTF-8 decode; a multipart expectation compares part by part, each
 * part located by the recording's own layout. A probe's sides stay strings —
 * the text where the bytes are UTF-8, standard base64 where they are not.
 */
import type { Body, Bundle, Flow, Pivot } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { confront } from "../src/confront.js"
import { signature } from "../src/probe.js"

const utf8 = new TextEncoder()
const crlf = (lines: ReadonlyArray<string>): string => `${lines.join("\r\n")}\r\n\r\n`
const b64 = (bytes: Uint8Array): string => Buffer.from(bytes).toString("base64")
const concat = (...parts: ReadonlyArray<Uint8Array>): Uint8Array => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
  let at = 0
  for (const p of parts) {
    out.set(p, at)
    at += p.length
  }
  return out
}

const BLOB_TYPE = "application/vnd.example.blob"
/** Seven bytes, four of them never valid UTF-8. */
const BLOB = Uint8Array.from([0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x00])
const OTHER = Uint8Array.from([0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x01])
const BIN_REF = "resources/uas1_r0_0.bin"
const SDP_REF = "resources/uas1_r0_0.sdp"
const XML_REF = "resources/uas1_r0_0.xml"
const XML = '<?xml version="1.0" encoding="utf-8"?>\r\n<request><play><prompt><audio url="a.wav"/></prompt></play></request>'
const OFFER =
  "v=0\r\no=- 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\n" +
  "m=audio 6000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=ptime:20\r\na=sendrecv\r\n"

const step = (id: string, over: Partial<Flow.Step> = {}): Flow.Step => ({
  id,
  leg: "B",
  op: "expect",
  in_dialog: true,
  check: "record",
  msg: { method: "INFO" },
  delay: { ms: 0, from: "trigger" as Flow.Delay["from"], compressible: true, timer_linked: false },
  ...over
})

const verdict: Bundle.RunVerdict = { case: "c", lane: "fake", status: "ok", failures: [] }

const expecting = (body: Body.Body): Pivot.PivotV3 => ({
  pivot_version: 3,
  case: { id: "c", title: "t", family: "f", variant: "repro", origin: "capture", lanes: {} },
  identities: [],
  calls: [],
  endpoints: [],
  actors: [],
  legs: [],
  flow: [step("s9", { msg: { method: "INFO", body } })],
  timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
})

const head = (contentType: string, len: number): string =>
  crlf([
    "INFO sip:callee@127.0.0.1 SIP/2.0",
    "To: <sip:+331@h.fr>;tag=b",
    "CSeq: 2 INFO",
    `Content-Type: ${contentType}`,
    `Content-Length: ${len}`
  ])

/** A recorded reception in the arm its bytes call for, as the interpreter writes it. */
const recorded = (contentType: string, body: Uint8Array, layout?: Bundle.RecordedMessage["body"]): Bundle.RecordedMessage => {
  const wire = concat(utf8.encode(head(contentType, body.length)), body)
  const text = (() => {
    try {
      return new TextDecoder("utf-8", { fatal: true }).decode(wire)
    } catch {
      return undefined
    }
  })()
  const arm = text === undefined ? { head: head(contentType, body.length), body_b64: b64(body) } : { raw: text }
  return { seq: 1, dir: "in", at_us: 1200, step: "s9", ...arm, ...(layout === undefined ? {} : { body: layout }) } as Bundle.RecordedMessage
}

const run = (body: Body.Body, message: Bundle.RecordedMessage, resources: ReadonlyMap<string, Uint8Array>, media?: Bundle.MediaMode) =>
  confront({
    pivot: expecting(body),
    verdict,
    recordings: new Map([["B", [message]]]),
    resources,
    ...(media === undefined ? {} : { media })
  }).probes.filter((p) => p.probe.kind === "body")

describe("a frozen binary body", () => {
  const frozen: Body.ResourceBody = { ref: BIN_REF, mode: "frozen", "content-type": BLOB_TYPE }
  const resources = new Map([[BIN_REF, BLOB]])

  it("equal byte for byte produces nothing", () => {
    expect(run(frozen, recorded(BLOB_TYPE, BLOB), resources)).toEqual([])
  })

  it("one byte off is one probe whose sides are the bytes' base64", () => {
    const probes = run(frozen, recorded(BLOB_TYPE, OTHER), resources)
    expect(probes).toHaveLength(1)
    const probe = probes[0]!.probe
    expect(probe.kind === "body" && probe).toMatchObject({
      step: "s9",
      mediaType: BLOB_TYPE,
      compare: "exact",
      captured: [b64(BLOB)],
      replayed: [b64(OTHER)]
    })
    expect(signature(probe)).toBe(`body:${BLOB_TYPE}:request:INFO:in-dialog`)
  })

  it("a reception with no body at all is confronted, as the empty side", () => {
    const bodiless = recorded(BLOB_TYPE, new Uint8Array(0))
    const probes = run(frozen, bodiless, resources)
    expect(probes).toHaveLength(1)
    expect(probes[0]!.probe.kind === "body" && probes[0]!.probe).toMatchObject({ captured: [b64(BLOB)], replayed: [""] })
  })
})

describe("a frozen text body, over bytes", () => {
  const frozen: Body.ResourceBody = { ref: XML_REF, mode: "frozen", "content-type": "application/example+xml" }
  const resources = new Map([[XML_REF, utf8.encode(XML)]])

  it("is still compared as text, both sides shown as the text they are", () => {
    expect(run(frozen, recorded("application/example+xml", utf8.encode(XML)), resources)).toEqual([])
    const retargeted = XML.replace("a.wav", "b.wav")
    const probes = run(frozen, recorded("application/example+xml", utf8.encode(retargeted)), resources)
    expect(probes).toHaveLength(1)
    expect(probes[0]!.probe.kind === "body" && probes[0]!.probe).toMatchObject({
      compare: "exact",
      captured: [XML],
      replayed: [retargeted]
    })
  })

  it("`xml` folds both sides after a strict decode; a side that is not UTF-8 is a probe", () => {
    const reflowed = "<request>\r\n  <play>\r\n    <prompt><audio url=\"a.wav\"/></prompt>\r\n  </play>\r\n</request>\r\n"
    expect(run({ ...frozen, compare: "xml" }, recorded("application/example+xml", utf8.encode(reflowed)), resources)).toEqual([])
    const probes = run({ ...frozen, compare: "xml" }, recorded("application/example+xml", BLOB), resources)
    expect(probes).toHaveLength(1)
    expect(probes[0]!.probe.kind === "body" && probes[0]!.probe.replayed).toEqual([b64(BLOB)])
  })

  it("`sdp` folds both sides under the mask, as before", () => {
    const described: Body.ResourceBody = { ref: SDP_REF, rewrite: ["c=addr", "m=port"], compare: "sdp" }
    const sdp = new Map([[SDP_REF, utf8.encode(OFFER)]])
    const rebooked = OFFER.replace("c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2").replace("m=audio 6000", "m=audio 40000")
    expect(run(described, recorded("application/sdp", utf8.encode(rebooked)), sdp)).toEqual([])
    const probes = run(described, recorded("application/sdp", utf8.encode(OFFER.replace("a=ptime:20", "a=ptime:30"))), sdp)
    expect(probes.map((p) => signature(p.probe))).toEqual(["body:sdp:m0:a=ptime:request:INFO:in-dialog"])
  })
})

describe("a multipart expectation, part by part", () => {
  const framing1 = "--b1\r\nContent-Type: application/sdp\r\nContent-ID: <offer@example.invalid>\r\n\r\n"
  const framing2 = "\r\n--b1\r\nContent-Type: application/vnd.example.blob\r\nContent-Transfer-Encoding: binary\r\n\r\n"
  const closing = "\r\n--b1--\r\n"

  /** The received body and its layout, as the recording states them. */
  const reception = (sdp: string, blob: Uint8Array, extra?: Uint8Array) => {
    const pieces: Array<Uint8Array> = [utf8.encode(framing1), utf8.encode(sdp), utf8.encode(framing2), blob]
    const parts: Array<{ content_type: string; content_id?: string; offset: number; len: number }> = [
      { content_type: "application/sdp", content_id: "<offer@example.invalid>", offset: framing1.length, len: sdp.length },
      { content_type: BLOB_TYPE, offset: framing1.length + sdp.length + framing2.length, len: blob.length }
    ]
    if (extra !== undefined) {
      const framing3 = "\r\n--b1\r\nContent-Type: application/vnd.example.other\r\n\r\n"
      pieces.push(utf8.encode(framing3), extra)
      const at = parts[1]!.offset + blob.length + framing3.length
      parts.push({ content_type: "application/vnd.example.other", offset: at, len: extra.length })
    }
    pieces.push(utf8.encode(closing))
    const body = concat(...pieces)
    return recorded("multipart/mixed;boundary=b1", body, { content_type: "multipart/mixed", len: body.length, parts } as Bundle.RecordedMessage["body"])
  }

  const expected: Body.Body = {
    multipart: {
      "content-type": "multipart/mixed",
      parts: [
        { "content-type": "application/sdp", ref: SDP_REF, rewrite: ["c=addr", "m=port"], compare: "sdp", "content-id": "<offer@example.invalid>" },
        { "content-type": BLOB_TYPE, ref: BIN_REF, mode: "frozen" }
      ]
    }
  } as unknown as Body.Body
  const resources = new Map<string, Uint8Array>([
    [SDP_REF, utf8.encode(OFFER)],
    [BIN_REF, BLOB]
  ])

  it("equal part by part produces nothing, the SDP part under its mask", () => {
    const rebooked = OFFER.replace("c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2").replace("m=audio 6000", "m=audio 40000")
    expect(run(expected, reception(rebooked, BLOB), resources)).toEqual([])
  })

  it("a differing SDP part is one probe under the fold, signed by section and key", () => {
    const probes = run(expected, reception(OFFER.replace("a=ptime:20", "a=ptime:30"), BLOB), resources)
    expect(probes.map((p) => signature(p.probe))).toEqual(["body:sdp:m0:a=ptime:request:INFO:in-dialog"])
  })

  it("a differing binary part is one probe whose sides are base64", () => {
    const probes = run(expected, reception(OFFER, OTHER), resources)
    expect(probes).toHaveLength(1)
    expect(probes[0]!.probe.kind === "body" && probes[0]!.probe).toMatchObject({
      mediaType: BLOB_TYPE,
      compare: "exact",
      captured: [b64(BLOB)],
      replayed: [b64(OTHER)]
    })
  })

  it("a part-count mismatch is one probe of its own", () => {
    const probes = run(expected, reception(OFFER, BLOB, Uint8Array.from([0x41, 0x42])), resources)
    expect(probes).toHaveLength(1)
    expect(probes[0]!.probe.kind).toBe("body")
  })
})
