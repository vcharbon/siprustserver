/**
 * What the case drawer says of a case's tags: it replays whatever it carries,
 * showing what they warn of, and says why the newest whole run left it out,
 * claiming nothing of runs that never ran it.
 */
import { renderToStaticMarkup } from "react-dom/server"
import { describe, expect, it } from "vitest"
import type { Campaign, CampaignRun } from "../src/api.ts"
import { CaseHistory } from "../src/components/CaseHistory.tsx"

const CASE = "Family/case"
const REASON = "excluded: harsh on real (--allow harsh runs it, --case runs it on its own)"

const run: CampaignRun = {
  id: 1,
  env: null,
  startedAt: "2026-11-06T00:00:00.000Z",
  finishedAt: null,
  labels: [],
  scope: null,
  selection: { allow: [], lanes: { real: ["harsh"] } },
  checked: {},
  lanes: [{
    lane: "real",
    runId: 1,
    session: "run1",
    status: "done",
    detail: null,
    skipped: 1,
    excluded: [{ caseId: CASE, reason: REASON }],
    results: []
  }]
}

const view: Campaign = {
  capture: "c",
  title: null,
  env: null,
  envs: [],
  saidLabel: "said",
  saidLog: "said-log",
  sides: [],
  lanes: ["real"],
  runs: [run],
  cases: [{
    caseId: CASE,
    ref: "Family/case",
    subcase: null,
    family: "Family",
    title: "A case",
    description: null,
    groups: ["harsh"],
    tags: ["harsh"],
    links: [],
    source: null
  }],
  launch: {
    lanes: [{ lane: "real", group: "g", refused: null }],
    tags: [{ tag: "harsh", label: "Harsh", warning: "May degrade", excludedOn: ["real"], allowed: false }],
    defaultLanes: null
  },
  own: null
}

const drawer = (read: Campaign): string =>
  renderToStaticMarkup(<CaseHistory view={read} caseId={CASE} lanes={["real"]} onClose={() => undefined} onDelta={() => undefined} />)

describe("the drawer of a case a tag left out", () => {
  it("offers its replay and shows what its tag warns of", () => {
    const markup = drawer(view)
    expect(markup).toContain("Replay")
    expect(markup).toContain("Harsh · May degrade")
  })

  it("says why, and states no flip for a case no run ran", () => {
    const markup = drawer(view)
    expect(markup).toContain(`real · ${REASON}`.replaceAll("<", "&lt;"))
    expect(markup).not.toContain("every run")
  })
})
