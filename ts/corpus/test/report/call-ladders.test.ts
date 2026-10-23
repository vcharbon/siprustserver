/**
 * A call's ladders as the viewer draws them: the replay ladder off a cell's
 * recording, and a value that is no v3 pivot drawing no reference ladder.
 */
import { describe, expect, it } from "vitest"
import * as CallLadders from "../../src/report/call-ladders.js"

const INVITE = JSON.stringify({
  seq: 1,
  dir: "out",
  at_us: 1000,
  step: "s1",
  raw: "INVITE sip:b@example.invalid SIP/2.0\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
})

describe("CallLadders", () => {
  it("draws a cell's replay ladder under its title, with the message it recorded", () => {
    const ladder = CallLadders.replayLadder({ recording: { A: `${INVITE}\n` }, pivot: null }, "run2 cap-case1")
    expect(ladder.some((line) => line.includes("run2 cap-case1"))).toBe(true)
    expect(ladder.some((line) => line.includes("INVITE"))).toBe(true)
  })

  it("reads only a v3 pivot as one, and draws no reference ladder off anything else", () => {
    expect(CallLadders.pivotOf({ pivot_version: 3 })).toEqual({ pivot_version: 3 })
    expect(CallLadders.pivotOf({ pivot_version: 2 })).toBeUndefined()
    expect(CallLadders.selectedLegs(null)).toEqual([])
    expect(CallLadders.referenceLadder({}, { pivot_version: 2 }, "t")).toBeUndefined()
  })
})
