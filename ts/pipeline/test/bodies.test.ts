/**
 * The expect side of the body registry: a frozen text body is stored and
 * asserted by content, SDP and multipart by shape, absence as its own claim,
 * and a binary payload stays undeclared. The send side is covered where the
 * flow is synthesized (`flowsteps.test.ts`).
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { expectBody } from "../src/bodies.js"
import { synthesize } from "../src/flowsteps.js"
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
    expect(stored.resources).toEqual([{ relPath: "resources/uac1_r3_0.xml", text: XML }])
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

  it("asserts SDP, multipart and absence by shape, with no resource", () => {
    const sdp = response({
      callId: CALLER_CALL_ID, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE",
      src: sut, dst: caller, ts_ms: 1_000, toTag: "sut-tag", sdp: "v=0\r\n"
    })
    expect(expectBody(sdp, "uac1_r0")).toEqual({ body: { mode: "sdp-present" }, resources: [], flags: [] })
    const mixed = info({ contentType: 'multipart/mixed;boundary="b"', text: "--b\r\n--b--\r\n" })
    expect(expectBody(mixed, "uac1_r0")).toEqual({ body: { mode: "multipart-present" }, resources: [], flags: [] })
    const bare = request({ callId: CALLER_CALL_ID, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 1_005, toTag: "sut-tag" })
    expect(expectBody(bare, "uac1_r0")).toEqual({ body: { mode: "absent" }, resources: [], flags: [] })
  })

  it("leaves a binary payload undeclared, whatever its type", () => {
    const text = info()
    const { raw, ...rest } = text as Flows.Msg & { raw: string }
    const cut = raw.indexOf("\r\n\r\n")
    const binary: Flows.Msg = {
      ...rest,
      head: raw.slice(0, cut),
      body_b64: Buffer.from(raw.slice(cut + 4), "latin1").toString("base64")
    } as Flows.Msg
    expect(expectBody(binary, "uac1_r0")).toEqual({ body: undefined, resources: [], flags: [] })
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
    const byPath = new Map(flow.resources.map((r) => [r.relPath, r.text]))
    expect(byPath.get(refOf(sent))).toBe(XML)
    expect(byPath.get(refOf(expected))).toBe(XML)
    expect(flow.resources.map((r) => r.relPath)).toHaveLength(new Set(flow.resources.map((r) => r.relPath)).size)
  })
})
