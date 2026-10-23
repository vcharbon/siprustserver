/**
 * What a cell's capture ladder opens on, in order of precedence: the bundle's
 * pivot, else the case's cut document — a cell the lane skipped has no bundle
 * — else the call's exclusion record, else nothing.
 */
import { describe, expect, it } from "vitest"
import { cellDocument, cellOpening, NO_OPENING } from "../src/opening.ts"

const CASE = "capture_demo-case1"

/** A v3 document whose steps cite `legs`, the SUT read off them; `sut` endpoints state an address where given. */
const document = (legs: readonly number[], sut: readonly string[] = []) => ({
  pivot_version: 3,
  case: { id: CASE },
  endpoints: [
    { id: "ep0", side: "peer", observed: "10.0.0.7:5060" },
    ...sut.map((observed, index) => ({ id: `sut${index}`, side: "sut", observed }))
  ],
  actors: [],
  legs: [],
  calls: [],
  flow: legs.map((leg, index) => ({
    id: `s${index + 1}`,
    op: index === 0 ? "send" : "expect",
    leg: "A",
    observed: { leg, msg: 0, at_us: index }
  }))
})

const exclusions = {
  path: "/nowhere/excluded.json",
  document: [{
    caseId: CASE,
    reason: "sut-18x-not-relayed",
    legs: [7, 8],
    callIds: ["r1"],
    sut: ["10.0.0.2"],
    evidence: [{ leg: 8, hop: 0, callId: "r1", msg: 0, detail: "took the 180 and sent no 18x of its own" }],
    line: `case '${CASE}' EXCLUDED sut-18x-not-relayed`
  }]
}

describe("the document a cell is drawn from", () => {
  it("is the bundle's pivot where the cell holds one", () => {
    const read = cellDocument(document([1]), document([2]))
    expect(read.ok && [...cellOpening(read, exclusions, CASE).selection!.legs]).toEqual([1])
  })

  it("is the case's cut document for a cell the lane skipped, which holds no bundle", () => {
    const read = cellDocument(undefined, document([5967, 5969, 6527], ["62.229.32.38:5060"]))
    expect(read.ok).toBe(true)
    const opening = cellOpening(read, exclusions, CASE)
    expect([...opening.selection!.legs]).toEqual([5967, 5969, 6527])
    expect(opening.sut).toEqual(["62.229.32.38:5060"])
  })

  it("reads as no document, with the reason, where neither the bundle nor the cut holds one", () => {
    expect(cellDocument(undefined, null)).toEqual({ ok: false, reason: "not a JSON object" })
  })
})

describe("what a cell with no document opens on", () => {
  it("is the call's own exclusion record", () => {
    const opening = cellOpening(cellDocument(undefined, null), exclusions, CASE)
    expect([...opening.selection!.legs]).toEqual([7, 8])
    expect(opening.sut).toEqual(["10.0.0.2"])
  })

  it("is another call's record never: the ladder opens on the capture's first legs", () => {
    expect(cellOpening(cellDocument(undefined, null), exclusions, "capture_demo-case2")).toBe(NO_OPENING)
  })

  it("is nothing where the study carries no records", () => {
    expect(cellOpening(cellDocument(undefined, undefined), undefined, CASE)).toBe(NO_OPENING)
  })
})
