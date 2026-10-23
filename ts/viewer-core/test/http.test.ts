/**
 * A run's HTTP side in the full text: each exchange where the run's own clock
 * puts it, beside the SIP bytes, both payloads inline.
 */
import { describe, expect, it } from "vitest"
import type { HttpExchange } from "../src/http/exchange.ts"
import { buildRecordingLayout } from "../src/recording/layout.ts"
import { parseRecording } from "../src/recording/parse.ts"
import { fullLadderToText } from "../src/text/full-text.ts"
import { document, expectRequest, send } from "./documents.ts"

const invite = [
  "INVITE sip:bob@example.test SIP/2.0",
  "From: <sip:alice@example.test>;tag=abc",
  "To: <sip:bob@example.test>",
  "CSeq: 1 INVITE",
  "Content-Type: application/sdp",
  "Content-Length: 21",
  "",
  "v=0",
  "m=audio 1 RTP/AVP 8"
].join("\r\n")

const exchange = (fields: Partial<HttpExchange> & Pick<HttpExchange, "ordinal" | "atMs" | "path">): HttpExchange => ({
  dst: "10.0.0.9:8081",
  method: "POST",
  requestHeaders: [],
  requestBody: undefined,
  status: 200,
  responseHeaders: [],
  responseBody: undefined,
  error: undefined,
  source: {},
  ...fields
})

const EXCHANGES: readonly HttpExchange[] = [
  exchange({
    ordinal: 1,
    atMs: 1,
    path: "/route?debug=true",
    requestHeaders: [{ name: "Content-Type", value: "application/json" }],
    requestBody: { ref: "r1" },
    responseBody: { decision: { kind: "ROUTE" } }
  }),
  exchange({ ordinal: 2, atMs: 90, path: "/route/release", requestBody: { ref: "r1" } })
]

describe("fullLadderToText", () => {
  const doc = document([send("s1", "A", "INVITE"), expectRequest("s2", "B", "INVITE")])
  const recording = parseRecording({
    A: JSON.stringify({ seq: 1, dir: "out", at_us: 500, step: "s1", raw: invite }),
    B: JSON.stringify({ seq: 1, dir: "in", at_us: 20_000, raw: invite })
  })
  const layout = buildRecordingLayout(recording, { document: doc, title: "replay x" })

  it("puts each SIP message's own bytes under its ladder line", () => {
    const text = fullLadderToText(layout, [])
    expect(text).toContain("    v=0")
    expect(text).toContain("    m=audio 1 RTP/AVP 8")
    expect(text.split("\n").filter((line) => line.trim() === "v=0")).toHaveLength(2)
  })

  it("puts each exchange in ladder position, with both payloads inline", () => {
    const lines = fullLadderToText(layout, EXCHANGES).split("\n")
    const at = (needle: string): number => {
      const index = lines.findIndex((line) => line.includes(needle))
      expect(index, needle).toBeGreaterThanOrEqual(0)
      return index
    }
    // The first consult lands between the INVITE the SUT took in and the one
    // it sent on; the second follows both.
    expect(at("m0 ")).toBeLessThan(at("http#1"))
    expect(at("http#1")).toBeLessThan(at("m1 "))
    expect(at("m1 ")).toBeLessThan(at("http#2"))
    expect(lines[at("http#1")]).toContain("POST /route?debug=true -> 200")
    expect(lines).toContain("    request Content-Type: application/json")
    expect(lines).toContain(`        "ref": "r1"`)
    expect(lines).toContain(`          "kind": "ROUTE"`)
  })

  it("states a consult nothing answered as the failure it was", () => {
    const failed = [exchange({ ordinal: 1, atMs: 0, path: "/route", status: undefined, error: "connection cut" })]
    expect(fullLadderToText(layout, failed)).toContain("POST /route -> no answer (connection cut)")
  })

  it("states the server's process time beside the answer, where the server stated one", () => {
    const timed = [exchange({ ordinal: 1, atMs: 0, path: "/route", processUs: 1874 })]
    expect(fullLadderToText(layout, timed)).toContain("POST /route -> 200 in 1.874 ms")
    const untimed = [exchange({ ordinal: 1, atMs: 0, path: "/route" })]
    expect(fullLadderToText(layout, untimed)).not.toMatch(/-> 200 in /)
  })
})
