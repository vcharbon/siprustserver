/** The summary page's place in the URL: written and read back the same; a report's case link opens its delta. */
import { describe, expect, it } from "vitest"
import type { Campaign } from "../src/api.ts"
import { type CampaignState, paramsOf, readState, runOf, summaryOf, vsOf } from "../src/campaign-state.ts"
import { href } from "../src/router.ts"

const query = (path: string) => new URLSearchParams(path.slice(path.indexOf("?")))

describe("the summary page's place", () => {
  it("reads back as it was written", () => {
    const state: CampaignState = {
      env: "env-a",
      tab: "grid",
      run: 7,
      vs: 5,
      filters: { lanes: ["lane-x"], failing: true, oneLane: false, fresh: true, text: "pai" },
      selection: {
        checks: ["B1 INVITE pai.uri", "rfc rule"],
        groups: ["group-a", "group b"],
        families: ["fam-a"],
        statuses: ["fail", "fail, still verifying"],
        lines: ["cdr fail", "call fail waived"]
      },
      caseId: "fam-a/one [s]",
      delta: { run: 5, lane: "lane-x" }
    }
    expect(readState(query(href.campaign("c", paramsOf(state))))).toEqual(state)
  })

  it("opens the delta a report's link names by its run, lane and case", () => {
    const read = readState(query("/campaign/c?run=7&lane=lane-x&case=fam-a%2Fone"))
    expect([read.run, read.caseId, read.delta]).toEqual([7, "fam-a/one", { run: 7, lane: "lane-x" }])
  })

  it("compares with the latest earlier run that judged a case the one read judged", () => {
    const run = (id: number, caseIds: ReadonlyArray<string>) => ({
      id,
      env: "env-a",
      startedAt: "",
      finishedAt: null,
      labels: [],
      checked: {},
    scope: null,
    selection: null,
      lanes: [{
        lane: "l",
        runId: id,
        session: `run${id}`,
        status: "done",
        detail: null,
        skipped: 0,
        excluded: [],
        results: caseIds.map((caseId) => ({ caseId, outcome: "pass" as const, cell: null, log: null, detail: null, checks: [] }))
      }]
    })
    const view: Campaign = {
      capture: "c",
      title: null,
      env: "env-a",
      envs: ["env-a"],
      launch: null,
      own: null,
      saidLabel: "s",
      sides: [],
      lanes: ["l"],
      cases: [],
      runs: [run(4, ["a"]), run(3, ["b"]), run(2, ["a", "b"]), run(1, ["a"])]
    }
    const state = readState(query("/campaign/c?run=4"))
    expect(vsOf(view, state, runOf(view, state))).toBe(2)
  })

  it("reads its full runs, a partial run only where asked by id", () => {
    const run = (id: number, scope: string | null) => ({
      id, env: null, startedAt: "", finishedAt: null, labels: [], checked: {}, scope, selection: null, lanes: []
    })
    const view: Campaign = {
      capture: "c",
      title: null,
      env: null, envs: [], launch: null, own: null, saidLabel: "s", sides: [], lanes: [], cases: [],
      runs: [run(3, "Routing/x [a]"), run(2, null), run(1, null)]
    }
    expect(summaryOf(view, undefined).runs.map((r) => r.id)).toEqual([2, 1])
    expect(summaryOf(view, 3).runs.map((r) => r.id)).toEqual([3, 2, 1])
    expect(runOf(summaryOf(view, undefined), readState(new URLSearchParams("")))).toBe(2)
  })
})
