/**
 * The relay placement moves only what a relay can be: a provisional to an
 * INVITE, behind the emission carrying its session description. An ACK to a
 * non-2xx is hop-by-hop (RFC 3261 §17.1.1.3) and relays nothing, so no step of
 * a leg is moved ahead of an earlier step of that leg's own transaction.
 */
import { describe, expect, it } from "vitest"
import type { StepTiming } from "../src/delay.js"
import type { StepDraft } from "../src/draft.js"
import type { StepSource } from "../src/flowsteps.js"
import { placeRelaysAfterSources } from "../src/relay-placement.js"

const timing = (leg: string, emits: boolean, typeKey: string, ms: number, cseq: number, image: string): StepTiming =>
  ({ leg, emits, typeKey, ts_us: ms * 1000, cseq, timerLinked: false, image })

const placed = (timings: Array<StepTiming>) => {
  const steps = timings.map((t, i) => ({ id: `s${i + 1}`, leg: t.leg, op: t.emits ? "send" : "expect", msg: {} }) as unknown as StepDraft)
  const sources = timings.map((_, i) => ({ id: `s${i + 1}`, origLeg: 0, msgIdx: i }) as unknown as StepSource)
  const before = [...steps]
  placeRelaysAfterSources({ steps, timings, sources })
  return steps.map((s) => before.indexOf(s) + 1)
}

describe("ACKs of several transactions on two legs", () => {
  it("keeps every step in capture order: an ACK to a non-2xx relays nothing", () => {
    const timings = [
      timing("A", true, "req:ACK", 0, 1, "bare"),
      timing("B", false, "req:ACK", 2, 1, "bare"),
      timing("A", true, "req:ACK", 600, 2, "sdp:o=x 1 2 IN IP4 h"),
      timing("B", false, "req:ACK", 602, 2, "sdp:o=x 1 2 IN IP4 h"),
      timing("B", true, "req:INVITE", 900, 3, "bare"),
      timing("B", false, "resp:491:INVITE", 1_100, 3, "bare"),
      timing("B", false, "req:ACK", 1_280, 3, "bare")
    ]
    expect(placed(timings)).toEqual([1, 2, 3, 4, 5, 6, 7])
  })
})

describe("two relayed provisionals with a step of their leg between them", () => {
  it("swaps nothing across that step", () => {
    const sdp = "sdp:o=x 1 1 IN IP4 h"
    const timings = [
      timing("B", true, "resp:183:INVITE", 0, 1, sdp),
      timing("B", true, "resp:183:INVITE", 1, 1, "bare"),
      timing("A", false, "resp:183:INVITE", 2, 1, "bare"),
      timing("A", true, "req:PRACK", 3, 2, "bare"),
      timing("A", false, "resp:183:INVITE", 4, 1, sdp)
    ]
    expect(placed(timings)).toEqual([1, 2, 3, 4, 5])
  })
})

describe("two relays stamped before their sources", () => {
  /**
   * The vantage stamps the caller-side 183 with an answer, then a bare one,
   * before the callee sent either. The answering relay moves behind its
   * source, past the bare one, itself a provisional no listed emission
   * explains; the bare one, naming nothing, stays where it was stamped.
   */
  it("moves the answering relay behind its source", () => {
    const sdp = "sdp:o=x 1 1 IN IP4 h"
    const timings = [
      timing("A", false, "resp:183:INVITE", 100, 1, sdp),
      timing("A", false, "resp:183:INVITE", 101, 1, "bare"),
      timing("B", true, "resp:183:INVITE", 220, 1, "bare"),
      timing("B", true, "resp:183:INVITE", 221, 1, sdp)
    ]
    expect(placed(timings)).toEqual([2, 3, 4, 1])
  })
})
