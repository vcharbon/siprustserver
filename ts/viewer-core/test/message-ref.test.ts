/**
 * An audit's message reference lands on the ladder message its leg and seq
 * name, whatever order the ladder merged two legs sharing one instant in.
 */
import { describe, expect, it } from "vitest"
import { buildRecordingLayout, eventOfRef, messageRefOf, parseRecording } from "../src/recording/index.ts"

const line = (seq: number, dir: string, raw: string) => JSON.stringify({ seq, dir, at_us: 7, raw })
const sip = (head: string, callId: string, cseq: string) => `${head}\r\nCall-ID: ${callId}\r\nCSeq: ${cseq}\r\n\r\n`

describe("a message reference", () => {
  it("finds the message its leg and seq name, at the ladder's own number", () => {
    const recording = parseRecording({
      A: [line(1, "out", sip("INVITE sip:b SIP/2.0", "a", "30 INVITE")), line(2, "in", sip("SIP/2.0 100 Trying", "a", "30 INVITE"))].join("\n"),
      B: line(1, "in", sip("INVITE sip:c SIP/2.0", "b", "1 INVITE"))
    })
    const layout = buildRecordingLayout(recording, {})
    const ref = messageRefOf({ offending: 2, offending_message: { leg: "B", seq: 1 } })
    expect(ref).toEqual({ leg: "B", seq: 1 })
    const event = eventOfRef(layout, ref!)
    expect(event?.legId).toBe("B")
    expect(event?.label).toContain("INVITE")
    expect(eventOfRef(layout, { leg: "B", seq: 9 })).toBeUndefined()
    expect(messageRefOf({ offending: 2 })).toBeUndefined()
  })
})
