/**
 * The run grid (families × runs × lanes, each cell its row's worst verdict)
 * and one case's history (its verdict per run and lane, the labels that
 * changed between runs, and the flip table of every check it ever failed).
 */
import { describe, expect, it } from "vitest"
import type { Campaign, CampaignResult } from "../src/api.ts"
import { charDiff } from "../src/char-diff.ts"
import { grid, history } from "../src/campaign-history.ts"

const result = (caseId: string, outcome: CampaignResult["outcome"], keys: ReadonlyArray<string> = []): CampaignResult => ({
  caseId,
  outcome,
  cell: caseId,
  log: null,
  detail: outcome === "error" ? "init failed" : null,
  checks: keys.map((key) => ({ anchor: "B1 INVITE", field: key, expected: "x", seen: "y", said: null, side: "sut" }))
})

const UNGROUPED = { title: null, description: null, groups: [], links: [], source: null }

const VIEW: Campaign = {
  capture: "campaign",
  env: null,
  envs: [],
  launch: null,
  saidLabel: "decision said",
  sides: ["decision", "sut"],
  lanes: ["lane-x", "lane-y"],
  cases: [
    { caseId: "fam-a/one", ref: "fam-a/one", subcase: null, family: "fam-a", ...UNGROUPED },
    { caseId: "fam-a/two", ref: "fam-a/two", subcase: null, family: "fam-a", ...UNGROUPED },
    { caseId: "fam-b/three", ref: "fam-b/three", subcase: null, family: "fam-b", ...UNGROUPED }
  ],
  runs: [3, 2, 1].map((id) => ({
    id,
    env: null,
    startedAt: `2026-10-1${id}T02:00:00.000Z`,
    finishedAt: null,
    labels: [{ name: "seed", value: id === 3 ? "s2" : "s1" }, { name: "sut", value: "c1" }],
    checked: {},
    scope: null,
    lanes: [
      {
        lane: "lane-x",
        runId: id * 10,
        session: `run${id * 10}`,
        status: "done",
        detail: null,
        skipped: 0,
        results: id === 3
          ? [result("fam-a/one", "failure", ["pai.uri"]), result("fam-a/two", "pass"), result("fam-b/three", "pass")]
          : id === 2
          ? [result("fam-a/one", "failure", ["pai.uri", "to.uri"]), result("fam-a/two", "error"), result("fam-b/three", "pass")]
          : [result("fam-a/one", "pass"), result("fam-a/two", "pass")]
      },
      { lane: "lane-y", runId: id * 10 + 1, session: `run${id * 10 + 1}`, status: "done", detail: null, skipped: 0, results: [result("fam-a/one", "pass")] }
    ]
  }))
}

describe("the run grid", () => {
  it("shows each family's worst verdict per run and lane, oldest run first, failing over error over pass", () => {
    const g = grid(VIEW, ["lane-x"])
    expect(g.runs).toEqual([1, 2, 3])
    expect(g.rows.map((row) => [row.name, row.cells.map((c) => `${c.run}:${c.lane}:${c.worst}:${c.failing}/${c.ran}`)])).toEqual([
      ["fam-a", ["1:lane-x:pass:0/2", "2:lane-x:failure:2/2", "3:lane-x:failure:1/2"]],
      ["fam-b", ["1:lane-x:none:0/0", "2:lane-x:pass:0/1", "3:lane-x:pass:0/1"]]
    ])
    expect(g.rows[0]!.cases.map((c) => c.caseId)).toEqual(["fam-a/one", "fam-a/two"])
  })

  it("puts a case under each of its groups, under its family where it has none", () => {
    const grouped: Campaign = {
      ...VIEW,
      cases: VIEW.cases.map((c) =>
        c.caseId === "fam-a/one" ? { ...c, groups: ["group-1", "group-2"] } : c.caseId === "fam-a/two" ? { ...c, groups: ["group-1"] } : c
      )
    }
    const g = grid(grouped, ["lane-x"])
    expect(g.rows.map((row) => [row.name, row.cases.map((c) => c.caseId)])).toEqual([
      ["fam-b", ["fam-b/three"]],
      ["group-1", ["fam-a/one", "fam-a/two"]],
      ["group-2", ["fam-a/one"]]
    ])
  })
})

describe("a case's history", () => {
  it("states its verdict per run and lane, the labels that changed, and when each failed check flipped", () => {
    const h = history(VIEW, "fam-a/one", ["lane-x", "lane-y"])
    expect(h.runs).toEqual([1, 2, 3])
    expect(h.verdicts).toEqual({ "lane-x": ["pass", "failure", "failure"], "lane-y": ["pass", "pass", "pass"] })
    expect(h.changed).toEqual([[], [], ["seed"]])
    expect(h.flips.map((f) => [f.lane, f.key, f.marks.join(""), f.changedAt])).toEqual([
      ["lane-x", "B1 INVITE pai.uri", "✓✗✗", [2]],
      ["lane-x", "B1 INVITE to.uri", "✓✗✓", [2, 3]]
    ])
  })
})

describe("the character diff", () => {
  it("marks what expected holds that seen lacks, and what seen holds that expected lacks", () => {
    expect(charDiff("sip:0123@h;user=phone", "sip:0123@h")).toEqual({
      expected: [{ text: "sip:0123@h", mark: false }, { text: ";user=phone", mark: true }],
      seen: [{ text: "sip:0123@h", mark: false }]
    })
    expect(charDiff("sip:0153@h", "sip:0193@h")).toEqual({
      expected: [{ text: "sip:01", mark: false }, { text: "5", mark: true }, { text: "3@h", mark: false }],
      seen: [{ text: "sip:01", mark: false }, { text: "9", mark: true }, { text: "3@h", mark: false }]
    })
  })

  it("marks the whole of a side the other lacks", () => {
    expect(charDiff("abc", null)).toEqual({ expected: [{ text: "abc", mark: true }], seen: [] })
  })
})
