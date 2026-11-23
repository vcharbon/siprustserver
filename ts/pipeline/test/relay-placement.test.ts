/**
 * The relay placement moves only what a relay can be: a provisional to an
 * INVITE, behind the emission carrying its session description or, on a
 * platform relaying provisionals as they arrive, the emission first delivering
 * the identifying values it carries. An ACK to a
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

const marked = (t: StepTiming, ...marks: Array<string>): StepTiming => ({ ...t, marks })

const placed = (timings: Array<StepTiming>, transparent18x = true) => {
  const steps = timings.map((t, i) => ({ id: `s${i + 1}`, leg: t.leg, op: t.emits ? "send" : "expect", msg: {} }) as unknown as StepDraft)
  const sources = timings.map((_, i) => ({ id: `s${i + 1}`, origLeg: 0, msgIdx: i }) as unknown as StepSource)
  const before = [...steps]
  placeRelaysAfterSources({ steps, timings, sources, transparent18x })
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

describe("an early answering relay and the steps it must not pass", () => {
  const x = "sdp:o=x 1 1 IN IP4 h"
  const y = "sdp:o=y 1 1 IN IP4 h"

  it("keeps two early answers in their order when their sources run the same way", () => {
    const timings = [
      timing("A", false, "resp:183:INVITE", 100, 1, x),
      timing("A", false, "resp:183:INVITE", 101, 1, y),
      timing("B", true, "resp:183:INVITE", 220, 1, x),
      timing("B", true, "resp:183:INVITE", 221, 1, y)
    ]
    expect(placed(timings)).toEqual([3, 1, 4, 2])
  })

  it("places three early answers each behind its own source, in order", () => {
    const z = "sdp:o=z 1 1 IN IP4 h"
    const timings = [
      timing("A", false, "resp:183:INVITE", 100, 1, x),
      timing("A", false, "resp:183:INVITE", 101, 1, y),
      timing("A", false, "resp:183:INVITE", 102, 1, z),
      timing("B", true, "resp:183:INVITE", 220, 1, x),
      timing("B", true, "resp:183:INVITE", 221, 1, y),
      timing("B", true, "resp:183:INVITE", 222, 1, z)
    ]
    expect(placed(timings)).toEqual([4, 1, 5, 2, 6, 3])
  })

  it("keeps an early 180 with an answer and an early 183 with another apart", () => {
    const timings = [
      timing("A", false, "resp:180:INVITE", 100, 1, x),
      timing("A", false, "resp:183:INVITE", 101, 1, y),
      timing("B", true, "resp:180:INVITE", 220, 1, x),
      timing("B", true, "resp:183:INVITE", 221, 1, y)
    ]
    expect(placed(timings)).toEqual([3, 1, 4, 2])
  })

  it("never passes a provisional the system under test made itself", () => {
    const timings = [
      timing("A", false, "resp:183:INVITE", 100, 1, x),
      timing("A", false, "resp:180:INVITE", 101, 1, "bare"),
      timing("B", true, "resp:183:INVITE", 220, 1, x)
    ]
    expect(placed(timings)).toEqual([1, 2, 3])
  })
})

describe("a bare relay stamped before the callee message whose values it carries", () => {
  /**
   * The caller-side 183 carries the callee side's identifying values 112 ms
   * before the callee's 183 delivers them; the callee's 180 and its relay are
   * stamped between. A bare message names no session, so those values alone
   * pair the 183 with its source. Each other case changes one thing of this
   * shape and keeps capture order.
   */
  const callee = ["x-trace:callee-1", "x-route:r-7"]
  const CAPTURED = [1, 2, 3, 4, 5, 6, 7, 8]
  const shape = (
    o: { early?: boolean; relayMarks?: Array<string>; sourceMarks?: Array<string>; sourceMs?: number; ringCseq?: number } = {}
  ) => [
    marked(timing("A", true, "req:INVITE", 0, 1, "bare"), "x-trace:caller-1"),
    timing("A", false, "resp:100:INVITE", 0.3, 1, "bare"),
    marked(timing("B", false, "req:INVITE", 16, 7, "bare"), "x-trace:caller-1"),
    marked(timing("B", true, "resp:100:INVITE", 17, 7, "bare"), ...(o.early === true ? callee : [])),
    marked(timing("A", false, "resp:183:INVITE", 73, 1, "bare"), ...(o.relayMarks ?? callee)),
    marked(timing("B", true, "resp:180:INVITE", 110, 7, "bare"), ...callee),
    marked(timing("A", false, "resp:180:INVITE", 111, o.ringCseq ?? 1, "bare"), ...callee),
    marked(timing("B", true, "resp:183:INVITE", o.sourceMs ?? 185, 7, "bare"), ...(o.sourceMarks ?? callee))
  ]

  it("moves it behind that message, past the one relayed 180 of its own leg", () => {
    expect(placed(shape())).toEqual([1, 2, 3, 4, 6, 7, 8, 5])
  })

  it("keeps it where a platform that rewrites its provisionals put it", () => {
    expect(placed(shape(), false)).toEqual(CAPTURED)
  })

  it("keeps a provisional carrying no identifying value", () => {
    expect(placed(shape({ relayMarks: [] }))).toEqual(CAPTURED)
  })

  it("keeps it where an earlier callee message delivered those values", () => {
    expect(placed(shape({ early: true }))).toEqual(CAPTURED)
  })

  it("keeps it where the later message delivers only some of them", () => {
    expect(placed(shape({ sourceMarks: [callee[0]!] }))).toEqual(CAPTURED)
  })

  it("keeps it where the later message is stamped more than the skew bound after it", () => {
    expect(placed(shape({ sourceMs: 574 }))).toEqual(CAPTURED)
  })

  it("never moves it past the callee leg's final", () => {
    const timings = shape()
    timings.splice(7, 0, timing("B", true, "resp:200:INVITE", 150, 7, "bare"))
    expect(placed(timings)).toEqual([...CAPTURED, 9])
  })

  it("never passes a provisional of its own leg that no listed emission explains", () => {
    const timings = shape()
    timings.splice(5, 1)
    expect(placed(timings)).toEqual([1, 2, 3, 4, 5, 6, 7])
  })

  it("never passes two relayed provisionals of its own leg", () => {
    const timings = shape()
    timings.splice(
      7,
      0,
      marked(timing("B", true, "resp:181:INVITE", 112, 7, "bare"), ...callee),
      marked(timing("A", false, "resp:181:INVITE", 113, 1, "bare"), ...callee)
    )
    expect(placed(timings)).toEqual([...CAPTURED, 9, 10])
  })

  it("never passes a relayed provisional of another transaction of its own leg", () => {
    expect(placed(shape({ ringCseq: 2 }))).toEqual(CAPTURED)
  })

  it("never passes a step of another transaction on the source's leg", () => {
    const timings = shape()
    timings.splice(7, 0, timing("B", false, "req:PRACK", 150, 8, "bare"))
    expect(placed(timings)).toEqual([...CAPTURED, 9])
  })

  it("never passes a step of a third leg", () => {
    const timings = shape()
    timings.splice(7, 0, timing("C", false, "req:INVITE", 150, 3, "bare"))
    expect(placed(timings)).toEqual([...CAPTURED, 9])
  })
})
