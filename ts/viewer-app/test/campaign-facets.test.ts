/**
 * The failures of one campaign run as facets: every failed check a facet, all
 * equal, a call counted under each check it fails; families beside them;
 * facets narrowing by AND; one check read by the shapes of what was seen.
 */
import { describe, expect, it } from "vitest"
import type { Campaign, CampaignCheck, CampaignResult } from "../src/api.ts"
import { ALL_FILTERS, facets, NO_SELECTION } from "../src/campaign-facets.ts"

const check = (anchor: string, field: string, seen: string | null, side = "sut"): CampaignCheck => ({
  anchor,
  field,
  expected: "x",
  seen,
  said: null,
  side
})

const ok = (caseId: string): CampaignResult => ({ caseId, outcome: "pass", cell: caseId, log: null, detail: null, checks: [] })
const failed = (caseId: string, ...checks: ReadonlyArray<CampaignCheck>): CampaignResult => ({
  caseId,
  outcome: "failure",
  cell: caseId,
  log: null,
  detail: null,
  checks
})
const errored = (caseId: string, detail: string): CampaignResult => ({
  caseId,
  outcome: "error",
  cell: null,
  log: null,
  detail,
  checks: []
})

const PAI = check("B1 INVITE", "pai.uri", "sip:+3312345678@192.0.2.1:5060")
const RURI = check("B1 INVITE", "ruri.uri", "sip:12345678@192.0.2.1", "decision")
const UUI = check("B1 INVITE", "header(User-to-User)", null, "decision")

const run = (id: number, lanes: Readonly<Record<string, ReadonlyArray<CampaignResult>>>) => ({
  id,
  env: null,
  startedAt: `2026-10-${10 + id}T02:00:00.000Z`,
  finishedAt: null,
  labels: [],
  checked: {},
    scope: null,
    selection: null,
  lanes: Object.entries(lanes).map(([lane, results], at) => ({
    lane,
    runId: id * 10 + at,
    session: `run${id * 10 + at}`,
    status: "done",
    detail: null,
    skipped: 2,
    excluded: [],
    results
  }))
})

const CASES = ["fam-a/one", "fam-a/two", "fam-b/three", "fam-b/four"]

/** The groups a case counts under: `fam-a/one` under two. */
const GROUPS: Readonly<Record<string, ReadonlyArray<string>>> = {
  "fam-a/one": ["group-1", "group-2"],
  "fam-a/two": ["group-1"]
}

/** Four runs, newest first: run 3 a pre-run error; PAI failing since run 2, URI new in run 4. */
const VIEW: Campaign = {
  capture: "campaign",
  env: null,
  envs: [],
  launch: null,
  own: null,
  saidLabel: "decision said",
  sides: ["decision", "sut"],
  lanes: ["lane-x", "lane-y"],
  cases: CASES.map((caseId) => ({
    caseId,
    ref: caseId,
    subcase: null,
    family: caseId.split("/")[0]!,
    title: caseId === "fam-a/one" ? "Number one" : null,
    description: null,
    groups: GROUPS[caseId] ?? [],
    tags: [],
    links: [],
    source: null
  })),
  runs: [
    run(4, {
      "lane-x": [failed("fam-a/one", PAI, RURI), failed("fam-a/two", PAI), ok("fam-b/three"), failed("fam-b/four", UUI)],
      "lane-y": [failed("fam-a/one", PAI), ok("fam-a/two"), ok("fam-b/three"), ok("fam-b/four")]
    }),
    run(3, { "lane-x": CASES.map((c) => errored(c, "database init failed")), "lane-y": CASES.map((c) => errored(c, "database init failed")) }),
    run(2, {
      "lane-x": [failed("fam-a/one", PAI), ok("fam-a/two"), failed("fam-b/three", UUI), ok("fam-b/four")],
      "lane-y": CASES.map(ok)
    }),
    run(1, { "lane-x": [ok("fam-a/one"), ok("fam-a/two"), failed("fam-b/three", check("A final", "status", "486")), ok("fam-b/four")], "lane-y": CASES.map(ok) })
  ]
}

const keyOf = (c: CampaignCheck) => `${c.anchor} ${c.field}`

describe("the failure facets of a run", () => {
  it("count a call under every check it fails, with the lanes and sides it failed on", () => {
    const f = facets(VIEW, 4, 2, ALL_FILTERS, { checks: [], groups: [], families: [] })
    expect(f.checks.map((c) => [c.key, c.calls, c.lanes, c.sides])).toEqual([
      [keyOf(PAI), 2, { "lane-x": 2, "lane-y": 1 }, { sut: 3 }],
      [keyOf(UUI), 1, { "lane-x": 1 }, { decision: 1 }],
      [keyOf(RURI), 1, { "lane-x": 1 }, { decision: 1 }]
    ])
    expect(f.families).toEqual([{ family: "fam-a", calls: 2 }, { family: "fam-b", calls: 1 }])
    expect(f.failing).toBe(3)
    expect(f.ran).toBe(4)
  })

  it("date a check from the first run of its streak, a run that judged no call keeping it", () => {
    const f = facets(VIEW, 4, 2, ALL_FILTERS, { checks: [], groups: [], families: [] })
    expect(Object.fromEntries(f.checks.map((c) => [c.key, c.since]))).toEqual({
      [keyOf(PAI)]: 2,
      [keyOf(UUI)]: 2,
      [keyOf(RURI)]: 4
    })
  })

  it("list the checks that pass again since the compared run, only once no call fails them", () => {
    const f = facets(VIEW, 4, 1, ALL_FILTERS, { checks: [], groups: [], families: [] })
    expect(f.passingAgain).toEqual([{ key: "A final status", calls: 1 }])
    expect(facets(VIEW, 4, 2, ALL_FILTERS, { checks: [], groups: [], families: [] }).passingAgain).toEqual([])
  })

  it("narrow the calls by AND over the selected checks and families, each call with all its failed checks", () => {
    const both = facets(VIEW, 4, 2, ALL_FILTERS, { checks: [keyOf(PAI), keyOf(RURI)], groups: [], families: [] })
    expect(both.matching.map((m) => m.caseId)).toEqual(["fam-a/one"])
    expect(both.matching[0]!.failed).toEqual([
      { key: keyOf(PAI), lanes: ["lane-x", "lane-y"] },
      { key: keyOf(RURI), lanes: ["lane-x"] }
    ])
    expect(facets(VIEW, 4, 2, ALL_FILTERS, { checks: [keyOf(PAI)], groups: [], families: ["fam-b"] }).matching).toEqual([])
    expect(facets(VIEW, 4, 2, ALL_FILTERS, { checks: [], groups: [], families: ["fam-b"] }).matching.map((m) => m.caseId))
      .toEqual(["fam-b/four"])
  })

  it("read one selected check by the shapes of what was seen, long digit runs and ports folded", () => {
    const f = facets(VIEW, 4, 2, ALL_FILTERS, { checks: [keyOf(PAI)], groups: [], families: [] })
    expect(f.shapes.map((s) => [s.shape, s.side, s.count])).toEqual([["sip:+#@192.0.2.1:#", "sut", 3]])
  })

  it("hold to the lanes and the filters asked", () => {
    const one = facets(VIEW, 4, 2, { ...ALL_FILTERS, lanes: ["lane-y"] }, { checks: [], groups: [], families: [] })
    expect(one.checks.map((c) => [c.key, c.calls])).toEqual([[keyOf(PAI), 1]])
    const single = facets(VIEW, 4, 2, { ...ALL_FILTERS, oneLane: true }, { checks: [], groups: [], families: [] })
    expect(single.matching.map((m) => m.caseId)).toEqual(["fam-a/two", "fam-b/four"])
    const fresh = facets(VIEW, 4, 2, { ...ALL_FILTERS, fresh: true }, { checks: [], groups: [], families: [] })
    expect(fresh.matching.map((m) => m.caseId)).toEqual(["fam-a/one", "fam-a/two", "fam-b/four"])
    const text = facets(VIEW, 4, 2, { ...ALL_FILTERS, text: "user-to" }, { checks: [], groups: [], families: [] })
    expect(text.matching.map((m) => m.caseId)).toEqual(["fam-b/four"])
  })

  it("state a run that judged no call as one error facet", () => {
    const f = facets(VIEW, 3, 2, ALL_FILTERS, { checks: [], groups: [], families: [] })
    expect(f.checks.map((c) => [c.key, c.calls, c.sides])).toEqual([["error database init failed", 4, { error: 8 }]])
  })

  it("read a call a run did not judge as unknown: no streak broken, nothing new, nothing passing again", () => {
    const only = (id: number, results: ReadonlyArray<CampaignResult>) => run(id, { "lane-x": results })
    const view: Campaign = {
      ...VIEW,
      lanes: ["lane-x"],
      runs: [
        only(3, [failed("fam-a/one", PAI), ok("fam-a/two")]),
        only(2, [ok("fam-a/two"), failed("fam-b/three", UUI)]),
        only(1, [failed("fam-a/one", PAI), ok("fam-a/two")])
      ]
    }
    const f = facets(view, 3, 2, ALL_FILTERS, { checks: [], groups: [], families: [] })
    expect(f.checks.map((c) => [c.key, c.since])).toEqual([[keyOf(PAI), 1]])
    expect(f.passingAgain).toEqual([])
    expect(facets(view, 3, 2, { ...ALL_FILTERS, fresh: true }, { checks: [], groups: [], families: [] }).matching).toEqual([])
  })

  it("count a failing call under each of its groups, and narrow to the calls of any group selected", () => {
    const f = facets(VIEW, 4, 2, ALL_FILTERS, NO_SELECTION)
    expect(f.groups).toEqual([{ group: "group-1", calls: 2 }, { group: "group-2", calls: 1 }])
    const narrowed = (groups: ReadonlyArray<string>) =>
      facets(VIEW, 4, 2, ALL_FILTERS, { ...NO_SELECTION, groups }).matching.map((m) => m.caseId)
    expect(narrowed(["group-2"])).toEqual(["fam-a/one"])
    expect(narrowed(["group-1", "group-2"])).toEqual(["fam-a/one", "fam-a/two"])
    expect(facets(VIEW, 4, 2, ALL_FILTERS, NO_SELECTION).matching[0]).toMatchObject({
      title: "Number one",
      groups: ["group-1", "group-2"]
    })
    expect(facets(VIEW, 4, 2, { ...ALL_FILTERS, text: "number one" }, NO_SELECTION).matching.map((m) => m.caseId))
      .toEqual(["fam-a/one"])
  })
})
