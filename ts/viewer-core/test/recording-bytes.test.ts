/**
 * A recorded line whose body is bytes: the interpreter writes it in the
 * extractor's `head` + `body_b64` form, and the viewer reads it the way it
 * reads a capture — the head's headers, the byte count, the body drawn as hex
 * where it is not text and part by part where the line carries a layout.
 */
import { describe, expect, it } from "vitest"
import { bodySegments } from "../src/flows/body.ts"
import { buildRecordingLayout } from "../src/recording/layout.ts"
import { parseRecording, parseRecordingLeg } from "../src/recording/parse.ts"
import { fullLadderToText } from "../src/text/full-text.ts"
import { ladderToText } from "../src/text/ladder-text.ts"
import { document, expectRequest, send } from "./documents.ts"
import { headBodyDocument, INDATA_DUMP, INFO_BODY_B64, INFO_DUMP, INFO_HEAD, INVITE_HEAD, MULTIPART_BODY_B64, SDP } from "./head-body.ts"

const invite = headBodyDocument.legs[0]!.msgs[0]!
const inviteLayout = invite.body

describe("a recorded line whose body is bytes", () => {
  const parsed = parseRecordingLeg(
    "B",
    [
      JSON.stringify({
        seq: 1,
        dir: "in",
        at_us: 200_000,
        step: "s3",
        head: INVITE_HEAD,
        body_b64: MULTIPART_BODY_B64,
        body: inviteLayout
      }),
      JSON.stringify({
        seq: 2,
        dir: "in",
        at_us: 1_100_000,
        step: "s11",
        head: INFO_HEAD,
        body_b64: INFO_BODY_B64,
        body: { content_type: "application/vnd.sample.indata", len: 18 }
      })
    ].join("\n")
  )
  const [multipart, info] = parsed.messages

  it("is read without a warning, its head's start line and headers intact", () => {
    expect(parsed.warnings).toEqual([])
    expect(parsed.messages).toHaveLength(2)
    expect(info?.method).toBe("INFO")
    expect(info?.cseq).toBe(2)
    expect(info?.cseqMethod).toBe("INFO")
    expect(info?.callId).toBe("call-a")
    expect(info?.toTag).toBe("xyz")
    expect(info?.headers.find((header) => header.name === "Content-Type")?.value).toBe("application/vnd.sample.indata")
    expect(info?.stepId).toBe("s11")
  })

  it("carries the datagram one character per byte, and counts its bytes as the wire does", () => {
    expect(info?.raw?.startsWith(INFO_HEAD)).toBe(true)
    expect(info?.raw?.length).toBe(INFO_HEAD.length + 18)
    expect(info?.raw?.charCodeAt(INFO_HEAD.length + 2)).toBe(0x47)
    expect(info?.bytes).toBe(309)
    expect(info?.binary).toBe(true)
    expect(multipart?.bytes).toBe(653)
    expect(multipart?.binary).toBe(true)
  })

  it("draws a single binary body as one hex dump", () => {
    expect(bodySegments(info!)).toEqual([{ kind: "hex", text: INFO_DUMP }])
  })

  it("draws a multipart body part by part off the line's own layout: framing, SDP as text, bytes as hex", () => {
    expect(multipart?.body?.parts.map((part) => [part.contentType, part.offset, part.len])).toEqual([
      ["application/sdp", 54, 85],
      ["application/vnd.sample.indata", 291, 17]
    ])
    const segments = bodySegments(multipart!)
    expect(segments.map((segment) => segment.kind)).toEqual(["framing", "text", "framing", "hex", "framing"])
    expect(segments[1]?.text).toBe(SDP)
    expect(segments[3]?.text).toBe(INDATA_DUMP)
  })

  it("renders on the ladder text as the message it is", () => {
    const doc = document([send("s10", "A", "INFO"), expectRequest("s11", "B", "INFO")])
    const recording = parseRecording({
      A: JSON.stringify({ seq: 1, dir: "out", at_us: 1_000_000, step: "s10", head: INFO_HEAD, body_b64: INFO_BODY_B64 }),
      B: JSON.stringify({ seq: 1, dir: "in", at_us: 1_100_000, step: "s11", head: INFO_HEAD, body_b64: INFO_BODY_B64 })
    })
    const lines = ladderToText(buildRecordingLayout(recording, { document: doc, title: "replay x" })).split("\n")
    expect(lines.filter((line) => line.startsWith("  m"))).toEqual([
      "  m0 +0.000000 uac1 -> SUT | INFO | CSeq 2 | fromA1 · toA1 | s10",
      "  m1 +0.100000 SUT -> uas1 | INFO | CSeq 2 | fromB1 · toB1 | s11"
    ])
  })

  it("renders on the full text export with its head and its body as a hex dump under the ladder line", () => {
    const doc = document([expectRequest("s11", "B", "INFO")])
    const recording = parseRecording({
      B: JSON.stringify({ seq: 1, dir: "in", at_us: 1_100_000, step: "s11", head: INFO_HEAD, body_b64: INFO_BODY_B64 })
    })
    const text = fullLadderToText(buildRecordingLayout(recording, { document: doc, title: "replay x" }), [])
    const firstHeadLine = INFO_HEAD.split("\r\n")[0]!
    expect(text).toContain(`    ${firstHeadLine}`)
    for (const line of INFO_DUMP.split("\n")) expect(text).toContain(`    ${line}`)
  })

  it("draws the layout-bounded body, and the bytes past the declared length as one segment labelled so", () => {
    // 18 declared bytes followed by a CRLF the datagram carried past them.
    const line = parseRecordingLeg(
      "B",
      JSON.stringify({ seq: 1, dir: "in", at_us: 0, step: "s11", head: INFO_HEAD, body_b64: "dhBHAIEMgQEFgweDEGUxU4cIDQo=", body: { content_type: "application/vnd.sample.indata", len: 18 } })
    ).messages[0]!
    expect(line.bytes).toBe(311)
    const segments = bodySegments(line)
    expect(segments[0]).toEqual({ kind: "hex", text: INFO_DUMP })
    expect(segments).toHaveLength(2)
    expect(segments[1]?.kind).toBe("excess")
    expect(segments[1]?.text).toMatch(/2 bytes? past the declared length/)
  })

  it("prints an LF-terminated head whole on the full export, whatever the binary body holds", () => {
    // The body carries CRLFCRLF: a reader that re-searched the blank line in
    // the rendered text would cut the head inside the body.
    const lfHead = INFO_HEAD.replace(/\r\n/g, "\n").replace("Content-Length: 18", "Content-Length: 8")
    const body = Uint8Array.from([0x76, 0x10, 0x0d, 0x0a, 0x0d, 0x0a, 0x81, 0xff])
    const doc = document([expectRequest("s11", "B", "INFO")])
    const recording = parseRecording({
      B: JSON.stringify({ seq: 1, dir: "in", at_us: 0, step: "s11", head: lfHead, body_b64: btoa(String.fromCharCode(...body)), body: { content_type: "application/vnd.sample.indata", len: 8 } })
    })
    const text = fullLadderToText(buildRecordingLayout(recording, { document: doc, title: "replay x" }), [])
    expect(text).toContain("    Content-Length: 8")
    expect(text).toContain("    0000  76 10 0d 0a 0d 0a 81 ff")
    expect(text).not.toContain("    v.")
  })

  it("keeps reading a text line as it always did", () => {
    const text = parseRecordingLeg("A", JSON.stringify({ seq: 1, dir: "out", at_us: 0, raw: "BYE sip:x SIP/2.0\r\nCSeq: 3 BYE\r\n\r\n" }))
    expect(text.messages[0]?.method).toBe("BYE")
    expect(text.messages[0]?.binary).toBe(false)
    expect(text.messages[0]?.bytes).toBe(34)
    expect(bodySegments(text.messages[0]!)).toEqual([])
  })
})
