/** An HTTP exchange follows the last ladder message observed at or before it went out, on the recording's clock. */
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

  it("is none for an exchange before the first message, or a ladder with none", () => {
    expect(exchangesAfter([{ atMs: -1 }], events)).toEqual([undefined])
    expect(exchangesAfter([{ atMs: 3 }], [])).toEqual([undefined])
  })
})
