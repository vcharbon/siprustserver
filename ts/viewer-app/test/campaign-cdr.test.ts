/**
 * A cell's CDRs in its call details: each declared call beside the lines seen
 * for the Call-ID its caller leg mints, seen calls no declaration names after,
 * no count where the cell holds no lines; the cells a delta
 * compares are the case's sub-cases on the cell's run and lane, or the case
 * on every lane of the run, each a cell the run wrote.
 */
import { describe, expect, it } from "vitest"
import type { Campaign, CampaignCase, CampaignResult } from "../src/api.ts"
import { callLines, declaredOf, deltaColumns } from "../src/campaign-cdr.ts"

const result = (caseId: string, cell: string | null): CampaignResult => ({
  caseId,
  outcome: "pass",
  cell,
  log: null,
  detail: null,
  checks: []
})

const kase = (ref: string, subcase: string | null): CampaignCase => ({
  caseId: subcase === null ? ref : `${ref} [${subcase}]`,
  ref,
  subcase,
  family: "f",
  title: null,
  description: null,
  groups: [], tags: [],
  links: [],
  source: null
})

const VIEW: Campaign = {
  capture: "c",
  title: null,
  env: null,
  envs: [],
  saidLabel: "said",
  sides: [],
  lanes: ["fake", "real"],
  launch: null,
  own: null,
  cases: [kase("R/a", "bye"), kase("R/a", "cancel"), kase("R/a", "prack"), kase("R/b", null)],
  runs: [{
    id: 9,
    env: null,
    startedAt: "",
    finishedAt: null,
    labels: [],
    scope: null, selection: null,
    checked: {},
    rawCdr: { "R/a [bye]": [{ call: "c1", leg: "A", lines: 1 }] },
    lanes: [
      {
        lane: "fake",
        runId: 1,
        session: "run1",
        status: "done",
        detail: null,
        skipped: 0, excluded: [],
        results: [result("R/a [bye]", "a--bye"), result("R/a [cancel]", null), result("R/a [prack]", "a--prack"), result("R/b", "b")]
      },
      { lane: "real", runId: 2, session: "run2", status: "done", detail: null, skipped: 0, excluded: [], results: [result("R/a [bye]", "a--bye")] }
    ]
  }]
}

describe("a cell's CDRs", () => {
  it("compares the case's sub-cases on the cell's lane, each that wrote a cell", () => {
    expect(deltaColumns(VIEW, "R/a [bye]", 9, "fake", "subcases")).toEqual([
      { label: "bye", session: "run1", cell: "a--bye", caseId: "R/a [bye]", lane: "fake" },
      { label: "prack", session: "run1", cell: "a--prack", caseId: "R/a [prack]", lane: "fake" }
    ])
  })

  it("compares the case on every lane of the run that wrote it a cell", () => {
    expect(deltaColumns(VIEW, "R/a [bye]", 9, "fake", "lanes").map((c) => [c.label, c.session])).toEqual([
      ["fake", "run1"],
      ["real", "run2"]
    ])
    expect(deltaColumns(VIEW, "R/a [prack]", 9, "fake", "lanes").map((c) => c.label)).toEqual(["fake"])
  })

  it("puts each declared call beside the lines seen for its leg, seen calls no declaration names after", () => {
    const declared = declaredOf(VIEW, "R/a [bye]", 9)
    expect(callLines(declared, [
      { leg: "A", callId: "A-1@h", lines: 3 },
      { leg: "held-A", callId: "held-A-2@h", lines: 1 }
    ])).toEqual([
      { call: "c1", leg: "A", callId: "A-1@h", declared: 1, seen: 3 },
      { call: null, leg: "held-A", callId: "held-A-2@h", declared: null, seen: 1 }
    ])
    expect(callLines(declared, [])).toEqual([{ call: "c1", leg: "A", callId: null, declared: 1, seen: 0 }])
    expect(callLines(declared, null)).toEqual([{ call: "c1", leg: "A", callId: null, declared: 1, seen: null }])
    expect(declaredOf(VIEW, "R/b", 9)).toBeUndefined()
  })
})
