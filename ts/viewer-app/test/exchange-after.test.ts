/** An HTTP exchange follows the last ladder message observed in or before the whole ms it went out at, on the recording's clock. */
import { buildRecordingLayout, parseRecording } from "@sip/viewer-core"
import { describe, expect, it } from "vitest"
import { exchangesAfter } from "../src/exchange-after.ts"
import { ladder } from "./ladder-fixture.ts"

describe("the message an exchange follows", () => {
  const events = ladder().events

  it("is the last message observed at or before the exchange", () => {
    expect(exchangesAfter([{ atMs: 1 }, { atMs: 16 }, { atMs: 39.9 }, { atMs: 41 }, { atMs: 5000 }], events)).toEqual([
      { id: "A#1", label: "INVITE" },
      { id: "B#1", label: "INVITE" },
      { id: "B#1", label: "INVITE" },
      { id: "B#2", label: "200 OK" },
      { id: "B#3", label: "200 OK" }
    ])
  })

  it("is a message observed in the same whole ms the exchange went out at, the exchange's time being rounded down", () => {
    const at = (atUs: number) => buildRecordingLayout(parseRecording({
      A: JSON.stringify({ seq: 1, dir: "out", at_us: 0, raw: "INVITE sip:b@example.test SIP/2.0\r\nCall-ID: a\r\nCSeq: 1 INVITE\r\n\r\n" }),
      B: JSON.stringify({ seq: 1, dir: "in", at_us: atUs, raw: "INVITE sip:b@example.test SIP/2.0\r\nCall-ID: b\r\nCSeq: 1 INVITE\r\n\r\n" })
    })).events
    expect(exchangesAfter([{ atMs: 16 }], at(16_400))).toEqual([{ id: "B#1", label: "INVITE" }])
    expect(exchangesAfter([{ atMs: 15 }], at(16_400))).toEqual([{ id: "A#1", label: "INVITE" }])
    expect(exchangesAfter([{ atMs: 0 }], at(111))).toEqual([{ id: "B#1", label: "INVITE" }])
  })

  it("is none for an exchange before the first message, or a ladder with none", () => {
    expect(exchangesAfter([{ atMs: -1 }], events)).toEqual([undefined])
    expect(exchangesAfter([{ atMs: 3 }], [])).toEqual([undefined])
  })
})
