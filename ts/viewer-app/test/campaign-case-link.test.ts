/** A case's link names the campaign, its environment, the run, the case and the delta, and opens that modal alone. */
import { describe, expect, it } from "vitest"
import { caseLink } from "../src/campaign-case-link.ts"
import { readState } from "../src/campaign-state.ts"

describe("the link to a case's call details", () => {
  const url = caseLink("http://10.0.0.9:5180", "camp a", { env: "env-a", caseId: "fam-a/one [s]", run: 7, lane: "lane-x" })

  it("is absolute and names only the campaign, environment, run, case and delta", () => {
    const at = new URL(url)
    expect(`${at.origin}${at.pathname}`).toBe("http://10.0.0.9:5180/campaign/camp%20a")
    expect([...at.searchParams.keys()].sort()).toEqual(["case", "delta", "env", "run"])
  })

  it("reads back as that modal, with no filter, tab or selection", () => {
    expect(readState(new URL(url).searchParams)).toEqual({
      env: "env-a",
      tab: "failures",
      run: 7,
      vs: undefined,
      filters: { lanes: [], failing: false, oneLane: false, fresh: false, text: "" },
      selection: { checks: [], groups: [], families: [], statuses: [], lines: [] },
      caseId: "fam-a/one [s]",
      delta: { run: 7, lane: "lane-x" }
    })
  })

  it("leaves the environment to the server where the view names none", () => {
    expect(new URL(caseLink("http://h", "c", { env: undefined, caseId: "x", run: 1, lane: "l" })).searchParams.has("env"))
      .toBe(false)
  })
})
