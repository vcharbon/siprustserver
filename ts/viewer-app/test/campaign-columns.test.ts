/**
 * The cells a campaign cell's records are compared with: the case's
 * sub-cases on the cell's run and lane, or the case on every lane of the run,
 * each a cell the run wrote, held by its lane run and case.
 */
import { describe, expect, it } from "vitest"
import type { Campaign, CampaignCase, CampaignResult } from "../src/api.ts"
import { compareColumns } from "../src/campaign-columns.ts"

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
  saidLog: "said-log",
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

describe("the cells a cell's records are compared with", () => {
  it("are the case's sub-cases on the cell's lane, each that wrote a cell", () => {
    expect(compareColumns(VIEW, "R/a [bye]", 9, "fake", "subcases")).toEqual([
      { label: "bye", run: 1, caseId: "R/a [bye]", session: "run1", cell: "a--bye", lane: "fake" },
      { label: "prack", run: 1, caseId: "R/a [prack]", session: "run1", cell: "a--prack", lane: "fake" }
    ])
  })

  it("are the case on every lane of the run that wrote it a cell", () => {
    expect(compareColumns(VIEW, "R/a [bye]", 9, "fake", "lanes").map((c) => [c.label, c.run])).toEqual([
      ["fake", 1],
      ["real", 2]
    ])
    expect(compareColumns(VIEW, "R/a [prack]", 9, "fake", "lanes").map((c) => c.label)).toEqual(["fake"])
  })
})
