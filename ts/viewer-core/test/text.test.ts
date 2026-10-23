import { describe, expect, it } from "vitest"
import { buildFlowsLayout } from "../src/flows/layout.ts"
import { parseFlowsDocument } from "../src/flows/parse.ts"
import { buildRecordingLayout } from "../src/recording/layout.ts"
import { parseRecording } from "../src/recording/parse.ts"
import { buildScenarioLayout } from "../src/layout/scenario.ts"
import { flowsToText } from "../src/text/flows-text.ts"
import { ladderToText } from "../src/text/ladder-text.ts"
import { document, expectRequest, send } from "./documents.ts"

const message = (
  tsUs: number,
  src: string,
  dst: string,
  summary: Record<string, unknown>,
  extra: Record<string, unknown> = {}
) => ({ ts_us: tsUs, src, dst, hop: 0, retx: false, raw: "INVITE sip:x SIP/2.0\r\n\r\n", summary, ...extra })

const flowsDocument = {
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
          cseq: { seq: 4, method: "INVITE" },
          from: { tag: "abc" },
          to: {}
        }),
        message(1_002_500, "2.2.2.2:5060", "1.1.1.1:5060", {
          kind: "response",
          status: 180,
          reason: "Ringing",
          cseq: { seq: 4, method: "INVITE" },
          from: { tag: "abc" },
          to: { tag: "xyz" }
        }, { retx: true })
      ]
    },
    {
      call_id: "call-b",
      msgs: [
        message(1_001_000, "2.2.2.2:5060", "3.3.3.3:5060", {
          kind: "request",
          method: "INVITE",
          uri: "sip:y@host",
          cseq: { seq: 1, method: "INVITE" },
          from: { tag: "def" }
        })
      ]
    }
  ],
  groups: [
    { legs: [0], t0_us: 1_000_000, final_status: 200 },
    { legs: [1], t0_us: 1_001_000 }
  ]
}

const invite = [
  "INVITE sip:+155500000000900004@example.test SIP/2.0",
  "Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK1",
  "From: <sip:0009001@example.test>;tag=abc",
  "To: <sip:+155500000000900004@example.test>",
  "Call-ID: call-1@example.test",
  "CSeq: 1 INVITE",
  "Content-Length: 0",
  "",
  ""
].join("\r\n")

describe("flowsToText", () => {
  const read = parseFlowsDocument(flowsDocument)
  if (!read.ok) throw new Error(read.reason)
  const layout = buildFlowsLayout(read.document, { title: "capture x" })

  it("renders the whole capture in the source.txt line shape", () => {
    expect(flowsToText(layout)).toBe([
      "capture x",
      "3 messages · 2 legs · 2 call-id families · no capture coordinates: every leg drawn as selected · " +
      "no SUT socket: this case cites no capture coordinate and the cut states no address · schema 5",
      "",
      "ladder (3 msgs; m<idx> +t leg<leg>/<msg> src -> dst | message | CSeq | from-tag/to-tag | bytes):",
      "  m0 +0.000000 leg0/0 1.1.1.1:5060 -> 2.2.2.2:5060 | INVITE sip:x@host | CSeq 4 INVITE | abc/- | 24B",
      "  m1 +0.001000 leg1/0 2.2.2.2:5060 -> 3.3.3.3:5060 | INVITE sip:y@host | CSeq 1 INVITE | def/- | 24B",
      "  m2 +0.002500 leg0/1 2.2.2.2:5060 -> 1.1.1.1:5060 | 180 Ringing | CSeq 4 INVITE | abc/xyz | 24B RETX",
      ""
    ].join("\n"))
  })

  it("carries the reader's own narrowing, and nothing it put away", () => {
    const narrowed = buildFlowsLayout(read.document, { title: "capture x", legs: [1] })
    const lines = flowsToText(narrowed).split("\n")
    expect(lines.filter((line) => line.startsWith("  m"))).toHaveLength(1)
    expect(lines[1]).toContain("filtered to leg 1")
  })
})

describe("ladderToText", () => {
  const doc = document([send("s1", "A", "INVITE"), expectRequest("s2", "B", "INVITE")])
  const recording = parseRecording({
    A: JSON.stringify({ seq: 1, dir: "out", at_us: 500, step: "s1", raw: invite }),
    B: JSON.stringify({ seq: 1, dir: "in", at_us: 2000, raw: invite })
  })

  it("renders a replay ladder with its observed times and step attribution", () => {
    const layout = buildRecordingLayout(recording, { document: doc, title: "replay x" })
    expect(ladderToText(layout)).toBe([
      "replay x",
      "2 legs · 2 messages",
      "",
      "ladder (2 events; m<idx> +t from -> to | message | detail | dialog | step):",
      "  m0 +0.000000 uac1 -> SUT | INVITE | CSeq 1 | fromA1 | s1",
      // A tagless To on the wire is no dialog yet, whatever the scenario declares.
      "  m1 +0.001500 SUT -> uas1 | INVITE | CSeq 1 | fromB1 | B#1",
      ""
    ].join("\n"))
  })

  it("shows a To-tag change as a new to-token", () => {
    const tagged = (start: string, cseq: string, toTag: string): string =>
      `${start}\r\nFrom: <sip:a@x>;tag=abc\r\nTo: <sip:b@x>;tag=${toTag}\r\nCSeq: ${cseq}\r\n\r\n`
    const changed = parseRecording({
      A: [
        JSON.stringify({ seq: 1, dir: "out", at_us: 0, step: "s1", raw: invite }),
        JSON.stringify({ seq: 2, dir: "in", at_us: 1000, raw: tagged("SIP/2.0 200 OK", "1 INVITE", "t1") }),
        JSON.stringify({ seq: 3, dir: "in", at_us: 2000, raw: tagged("SIP/2.0 481 Call/Transaction Does Not Exist", "1 CANCEL", "t2") })
      ].join("\n")
    })
    const lines = ladderToText(buildRecordingLayout(changed, { document: doc })).split("\n")
    expect(lines[5]).toBe("  m1 +0.001000 SUT -> uac1 | 200 OK | INVITE · CSeq 1 | fromA1 · toA1 | A#2")
    expect(lines[6]).toBe("  m2 +0.002000 SUT -> uac1 | 481 Call/Transaction Does Not Exist | CANCEL · CSeq 1 | fromA1 · toA2 | A#3")
  })

  it("leaves the time column out of a scenario, which states no observed time", () => {
    const text = ladderToText(buildScenarioLayout(doc))
    expect(text).toContain("ladder (2 events; m<idx> from -> to |")
    expect(text).toContain("  m0 uac1 -> SUT | INVITE")
    expect(text).not.toContain("+0.000000")
  })
})
