/** The summary page's place in the URL: written and read back the same; a report's case link opens its delta. */
import { describe, expect, it } from "vitest"
import { type CampaignState, paramsOf, readState } from "../src/campaign-state.ts"
import { href } from "../src/router.ts"

const query = (path: string) => new URLSearchParams(path.slice(path.indexOf("?")))

describe("the summary page's place", () => {
  it("reads back as it was written", () => {
    const state: CampaignState = {
      tab: "grid",
      run: 7,
      vs: 5,
      filters: { lanes: ["lane-x"], failing: true, oneLane: false, fresh: true, text: "pai" },
      selection: { checks: ["B1 INVITE pai.uri", "rfc rule"], families: ["fam-a"] },
      caseId: "fam-a/one [s]",
      delta: { run: 5, lane: "lane-x" }
    }
    expect(readState(query(href.campaign("c", paramsOf(state))))).toEqual(state)
  })

  it("opens the delta a report's link names by its run, lane and case", () => {
    const read = readState(query("/campaign/c?run=7&lane=lane-x&case=fam-a%2Fone"))
    expect([read.run, read.caseId, read.delta]).toEqual([7, "fam-a/one", { run: 7, lane: "lane-x" }])
  })
})
