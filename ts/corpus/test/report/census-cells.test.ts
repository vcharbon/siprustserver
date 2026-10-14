/**
 * A replay `--from-census <file> --rule R`: the cells a census rule's hits
 * name, read through the call groups each case document states, and every hit
 * no case of the cut holds said so rather than dropped.
 */
import { describe, expect, it } from "vitest"
import * as CensusCells from "../../src/report/census-cells.js"

const C = "trace.pcap.gz"
const D = "capture_d.pcap.gz"

const hit = (capture: string, group: number, callId: string, rule = "no-200-after-cancel", leg = 0) => ({
  capture,
  group,
  leg,
  call_id: callId,
  rule
})

/** A split family: two calls of one root Call-ID, cut from the same call group, each on its own legs. */
const SPLIT: ReadonlyMap<string, ReadonlyArray<CensusCells.CutCall>> = new Map([
  [C, [
    { caseId: `${C}-cut1330`, callKey: "r@h#ingress@100", groups: [89], legs: [10, 11] },
    { caseId: `${C}-cut1331`, callKey: "r@h#ingress@200", groups: [89], legs: [12, 13] }
  ]]
])

describe("CensusCells.cellsOf over a split family", () => {
  it("names the sibling whose legs hold the hit's leg", () => {
    const read = CensusCells.cellsOf([hit(C, 89, "r@h", "no-200-after-cancel", 12)], "no-200-after-cancel", SPLIT)
    expect(read.work).toEqual([{ capture: C, cases: [`${C}-cut1331`] }])
  })

  it("names every sibling holding the group where no sibling's legs hold the hit's leg", () => {
    const read = CensusCells.cellsOf([hit(C, 89, "x@h", "no-200-after-cancel", 99)], "no-200-after-cancel", SPLIT)
    expect(read.work).toEqual([{ capture: C, cases: [`${C}-cut1330`, `${C}-cut1331`] }])
  })

  it("tells the siblings apart by leg on the Call-ID fallback too", () => {
    const read = CensusCells.cellsOf([hit(C, 7, "r@h", "no-200-after-cancel", 10)], "no-200-after-cancel", SPLIT)
    expect(read.work).toEqual([{ capture: C, cases: [`${C}-cut1330`] }])
    const both = CensusCells.cellsOf([hit(C, 7, "r@h", "no-200-after-cancel", 50)], "no-200-after-cancel", SPLIT)
    expect(both.work).toEqual([{ capture: C, cases: [`${C}-cut1330`, `${C}-cut1331`] }])
  })
})

describe("CensusCells.flowsRefusal", () => {
  it("refuses, naming each capture, where the flows document the census read is not the one the cut read", () => {
    const why = CensusCells.flowsRefusal([
      { capture: C, document: "/a/C.flows.json", read: "h1", cut: "h1" },
      { capture: D, document: "/a/D.flows.json", read: "h2", cut: "h3" },
      { capture: "capture_e.pcap.gz", document: "/a/E.flows.json", read: null, cut: "h4" }
    ])
    expect(why).toContain(D)
    expect(why).toContain("capture_e.pcap.gz")
    expect(why).not.toContain(`${C}:`)
    expect(CensusCells.flowsRefusal([{ capture: C, document: "/a/C.flows.json", read: "h1", cut: "h1" }])).toBeNull()
  })
})

const CALLS: ReadonlyMap<string, ReadonlyArray<CensusCells.CutCall>> = new Map([
  [C, [
    { caseId: `${C}-case1`, callKey: "root-1@h", groups: [309] },
    { caseId: `${C}-case2`, callKey: "root-2@h#ingress@17", groups: [] },
    { caseId: `${C}-case3`, callKey: "root-3@h", groups: [12, 13] }
  ]],
  [D, [{ caseId: `${D}-auto`, callKey: "d@h", groups: [0] }]]
])

describe("CensusCells.cellsOf", () => {
  it("names the cases whose call groups hold a hit of the rule, per capture, each once", () => {
    const read = CensusCells.cellsOf(
      [hit(C, 309, "x"), hit(C, 13, "y"), hit(C, 12, "z"), hit(D, 0, "d@h"), hit(C, 1, "w", "other-rule")],
      "no-200-after-cancel",
      CALLS
    )
    expect(read.work).toEqual([
      { capture: C, cases: [`${C}-case1`, `${C}-case3`] },
      { capture: D, cases: [`${D}-auto`] }
    ])
    expect(read.unmatched).toEqual([])
  })

  it("falls back on the call key's root Call-ID where no group matches", () => {
    const read = CensusCells.cellsOf([hit(C, 99, "root-2@h")], "no-200-after-cancel", CALLS)
    expect(read.work).toEqual([{ capture: C, cases: [`${C}-case2`] }])
  })

  it("says which hits no case holds, by capture, group and Call-ID", () => {
    const read = CensusCells.cellsOf(
      [hit(C, 98, "lost@h"), hit("capture_uncut.pcap.gz", 1, "u@h"), hit(C, 97, "gone@h")],
      "no-200-after-cancel",
      CALLS
    )
    expect(read.work).toEqual([])
    expect(read.unmatched).toEqual([
      { capture: C, group: 98, callId: "lost@h", why: "no-case" },
      { capture: "capture_uncut.pcap.gz", group: 1, callId: "u@h", why: "no-cut" },
      { capture: C, group: 97, callId: "gone@h", why: "no-case" }
    ])
    expect(CensusCells.unmatchedLines(read.unmatched, 1)).toEqual([
      "1 capture(s) with hits have no standing cut: capture_uncut.pcap.gz",
      `${C}: 2 hit(s) no case of the cut holds (refused or never proposed), e.g. group 98 lost@h`
    ])
  })
})

describe("CensusCells.ruleRefusal", () => {
  it("refuses a rule the report never measured, naming the ones it did", () => {
    const why = CensusCells.ruleRefusal({ rules: { "no-200-after-cancel": {}, "unacked-reliable-provisional": {} } }, "nope")
    expect(why).toContain('"nope"')
    expect(why).toContain("no-200-after-cancel, unacked-reliable-provisional")
    expect(CensusCells.ruleRefusal({ rules: { "no-200-after-cancel": {} } }, "no-200-after-cancel")).toBeNull()
  })
})
