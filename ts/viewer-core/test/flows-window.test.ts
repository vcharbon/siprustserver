/**
 * Reading a flows document a window of legs at a time: which window a ladder
 * asks for, and what the document it draws holds. A capture of thousands of
 * calls is never one value here — neither on the wire nor in the ladder — so
 * the legs a window did not bring back are ABSENT, at indices that stay the
 * document's own.
 */
import { describe, expect, it } from "vitest"
import { callIdFamilies } from "../src/flows/families.ts"
import { buildFlowsLayout } from "../src/flows/layout.ts"
import { parseFlowsDocument } from "../src/flows/parse.ts"
import { EMPTY_SELECTION, messageCoordinate, type CaptureSelection } from "../src/flows/selection.ts"
import {
  chunkedFlows,
  LADDER_LEGS,
  ladderWindow,
  legWindows,
  SERVED_LEGS,
  SERVED_LEGS_MAX,
  servedWindow,
  shiftWindow,
  windowedFlows
} from "../src/flows/window.ts"

const envelope = { schema: 5, groups: [], legs: [] }

const legOf = (index: number) => ({
  call_id: `call-${index}@10.0.0.1`,
  final_status: 200,
  msgs: [{
    ts_us: 1_000_000 + index,
    src: "10.0.0.1:5060",
    dst: "10.0.0.2:5060",
    raw: "INVITE sip:x SIP/2.0\r\n\r\n",
    summary: { kind: "request", method: "INVITE", uri: "sip:x@host", cseq: { seq: 1, method: "INVITE" } }
  }]
})

describe("servedWindow", () => {
  it("serves its own page where the request states no count", () => {
    expect(servedWindow(30_100, 0)).toEqual({ from: 0, count: SERVED_LEGS })
  })

  it("caps what one request may ask for", () => {
    expect(servedWindow(30_100, 0, 30_000)).toEqual({ from: 0, count: SERVED_LEGS_MAX })
  })

  it("stops at the last leg the document holds", () => {
    expect(servedWindow(30_100, 30_095, 200)).toEqual({ from: 30_095, count: 5 })
  })

  it("answers an empty window at the end for a request past it", () => {
    expect(servedWindow(30_100, 3_000_000_000, 10)).toEqual({ from: 30_100, count: 0 })
  })

  it("answers an empty window for a document with no leg", () => {
    expect(servedWindow(0, 0)).toEqual({ from: 0, count: 0 })
  })
})

describe("legWindows", () => {
  it("reads a run of consecutive legs as one window, and legs apart as one each", () => {
    expect(legWindows(30_100, [6_527, 5_969, 5_967, 5_968])).toEqual([
      { from: 5_967, count: 3 },
      { from: 6_527, count: 1 }
    ])
  })

  it("asks for a leg once, and for a leg outside the document never", () => {
    expect(legWindows(100, [7, 7, -1, 100, 2.5])).toEqual([{ from: 7, count: 1 }])
  })

  it("asks for nothing where the case cites no leg", () => {
    expect(legWindows(100, [])).toEqual([])
  })

  it("splits a run longer than one response carries", () => {
    const run = Array.from({ length: SERVED_LEGS_MAX + 2 }, (_, index) => 10 + index)
    expect(legWindows(1_000, run)).toEqual([
      { from: 10, count: SERVED_LEGS_MAX },
      { from: 10 + SERVED_LEGS_MAX, count: 2 }
    ])
  })
})

describe("chunkedFlows", () => {
  it("holds each window's legs at the document's own indices, and nothing between them", () => {
    const value = chunkedFlows(envelope, 10, [
      { from: 2, legs: [legOf(2), legOf(3)] },
      { from: 8, legs: [legOf(8)] }
    ]) as { legs: unknown[] }
    expect(value.legs.length).toBe(10)
    expect([2, 3, 8].map((index) => value.legs[index] !== undefined)).toEqual([true, true, true])
    expect([0, 1, 4, 7, 9].map((index) => value.legs[index])).toEqual([undefined, undefined, undefined, undefined, undefined])
  })
})

describe("ladderWindow", () => {
  it("draws a capture that fits whole", () => {
    expect(ladderWindow(50, [])).toEqual({ from: 0, count: 50 })
  })

  it("draws the document's first legs where the ladder is about none of them", () => {
    expect(ladderWindow(30_100, [])).toEqual({ from: 0, count: LADDER_LEGS })
  })

  it("opens on the first leg the ladder is about", () => {
    expect(ladderWindow(30_100, [20_003, 20_000])).toEqual({ from: 20_000, count: LADDER_LEGS })
  })

  it("never asks past the last leg", () => {
    expect(ladderWindow(30_100, [30_095])).toEqual({ from: 30_095, count: 5 })
  })

  it("holds the budget where the legs it is about are wider than one window", () => {
    expect(ladderWindow(30_100, [10, 4_000])).toEqual({ from: 10, count: LADDER_LEGS })
  })

  it("asks for nothing of a document with no leg", () => {
    expect(ladderWindow(0, [])).toEqual({ from: 0, count: 0 })
  })

  it("refuses a window outside the document", () => {
    expect(ladderWindow(100, [-5])).toEqual({ from: 0, count: 100 })
    expect(ladderWindow(100, [500])).toEqual({ from: 0, count: 100 })
  })
})

/** A case citing one message of each of these legs. */
const cites = (...legs: readonly number[]): CaptureSelection => ({
  ...EMPTY_SELECTION,
  legs: new Set(legs),
  messages: new Set(legs.map((leg) => messageCoordinate(leg, 0))),
  empty: false
})

describe("shiftWindow", () => {
  it("moves a window on by its own width", () => {
    expect(shiftWindow({ from: 200, count: 200 }, 30_100, 1)).toEqual({ from: 400, count: 200 })
  })

  it("moves it back to the document's first leg and no further", () => {
    expect(shiftWindow({ from: 100, count: 200 }, 30_100, -1)).toEqual({ from: 0, count: 200 })
  })

  it("keeps the last window inside the document", () => {
    expect(shiftWindow({ from: 30_000, count: 200 }, 30_100, 1).from).toBeLessThan(30_100)
  })
})

describe("windowedFlows", () => {
  it("puts the window's legs at the indices the document gives them", () => {
    const value = windowedFlows(envelope, 30_100, 20_000, [legOf(20_000), legOf(20_001)])
    const read = parseFlowsDocument(value)
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs).toHaveLength(30_100)
    expect(read.document.legs[20_000]?.callId).toBe("call-20000@10.0.0.1")
    expect(read.document.legs[20_001]?.index).toBe(20_001)
    expect(read.document.messages.map((message) => message.legIndex)).toEqual([20_000, 20_001])
  })

  it("holds no leg at an index the window did not bring back", () => {
    const read = parseFlowsDocument(windowedFlows(envelope, 30_100, 20_000, [legOf(20_000)]))
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.legs[0]).toBeUndefined()
    expect(read.document.legs[19_999]).toBeUndefined()
    expect(read.document.warnings).toEqual([])
  })

  it("keeps the envelope's own fields", () => {
    const read = parseFlowsDocument(
      windowedFlows({ schema: 5, groups: [{ legs: [1, 2], evidence: [] }], legs: [] }, 3, 1, [legOf(1)])
    )
    if (!read.ok) throw new Error(read.reason)
    expect(read.document.schema).toBe(5)
    expect(read.document.groups[0]?.legs).toEqual([1, 2])
  })

  it("says nothing of a correlation naming a leg the window did not read", () => {
    const groups = [{ legs: [20_000, 25_000], evidence: [{ kind: "derived_call_id", legs: [20_000, 25_000] }] }]
    const read = parseFlowsDocument(
      windowedFlows({ schema: 5, groups, legs: [] }, 30_100, 20_000, [legOf(20_000)])
    )
    if (!read.ok) throw new Error(read.reason)
    expect(callIdFamilies(read.document).warnings).toEqual([])
  })

  it("still says so of a correlation naming a leg the document does not index", () => {
    const groups = [{ legs: [20_000, 40_000], evidence: [{ kind: "derived_call_id", legs: [20_000, 40_000] }] }]
    const read = parseFlowsDocument(
      windowedFlows({ schema: 5, groups, legs: [] }, 30_100, 20_000, [legOf(20_000)])
    )
    if (!read.ok) throw new Error(read.reason)
    expect(callIdFamilies(read.document).warnings).toEqual([{
      legs: [20_000, 40_000],
      text: "group 0: derived_call_id evidence names 20000, 40000, which this document does not hold"
    }])
  })

  it("says nothing of the legs a window opened on them HOLDS, in the line's own base", () => {
    // The line and the warning must count in ONE base: a window the ladder
    // opened on the case's legs holds them, so it has nothing to say about them.
    const total = 30_100
    const window = ladderWindow(total, [20_001, 20_002])
    const legs = Array.from({ length: window.count }, (_, at) => legOf(window.from + at))
    const read = parseFlowsDocument(windowedFlows(envelope, total, window.from, legs))
    if (!read.ok) throw new Error(read.reason)
    const layout = buildFlowsLayout(read.document, { selection: cites(20_001, 20_002), title: "big" })
    expect(layout.warnings.filter((one) => one.includes("outside the window drawn"))).toEqual([])
    // The line names a range that HOLDS the two legs, counted as the warning counts.
    expect(window.from).toBeLessThanOrEqual(20_001)
    expect(window.from + window.count).toBeGreaterThan(20_002)
    expect(read.document.legs[20_001]).toBeDefined()
    expect(read.document.legs[20_002]).toBeDefined()
  })

  it("names them once the reader pages past them, and only then", () => {
    const total = 30_100
    const moved = shiftWindow(ladderWindow(total, [20_001, 20_002]), total, 1)
    const legs = Array.from({ length: moved.count }, (_, at) => legOf(moved.from + at))
    const read = parseFlowsDocument(windowedFlows(envelope, total, moved.from, legs))
    if (!read.ok) throw new Error(read.reason)
    const layout = buildFlowsLayout(read.document, { selection: cites(20_001, 20_002), title: "big" })
    expect(moved.from).toBeGreaterThan(20_002)
    expect(layout.warnings[0]).toBe("2 legs of this case lie outside the window drawn: 20001, 20002")
  })

  it("says how many legs of the case the window drawn leaves out, before anything else", () => {
    // Beside a colouring note, which the pane shows six of: the legs the reader
    // opened the case for are what a window must never drop in silence.
    const derived = { ...legOf(6), call_id: `1-${legOf(5).call_id}` }
    const read = parseFlowsDocument(windowedFlows(envelope, 30_100, 5, [legOf(5), derived]))
    if (!read.ok) throw new Error(read.reason)
    const layout = buildFlowsLayout(read.document, { selection: cites(5, 250, 900), title: "big" })
    expect(layout.warnings[0]).toBe("2 legs of this case lie outside the window drawn: 250, 900")
    expect(layout.warnings.length).toBeGreaterThan(1)
  })

  it("says nothing of the kind where the document was read whole", () => {
    const read = parseFlowsDocument(windowedFlows(envelope, 2, 0, [legOf(0), legOf(1)]))
    if (!read.ok) throw new Error(read.reason)
    const layout = buildFlowsLayout(read.document, { selection: cites(0, 1), title: "small" })
    expect(layout.warnings).toEqual([])
  })

  it("is read and drawn over the legs it holds, never the whole span", () => {
    // A window inside a capture of thousands: what costs per leg must cost per
    // leg READ, so a document 30,000 legs wide draws its 2 as fast as a
    // document of 2.
    const read = parseFlowsDocument(windowedFlows(envelope, 30_100, 20_000, [legOf(20_000), legOf(20_001)]))
    if (!read.ok) throw new Error(read.reason)
    const started = performance.now()
    const families = callIdFamilies(read.document)
    const layout = buildFlowsLayout(read.document, { title: "big" })
    expect(performance.now() - started).toBeLessThan(500)
    expect(families.families).toHaveLength(2)
    expect(layout.rows.map((row) => row.legIndex)).toEqual([20_000, 20_001])
    expect(layout.legs.map((leg) => leg.legIndex)).toEqual([20_000, 20_001])
  })
})
