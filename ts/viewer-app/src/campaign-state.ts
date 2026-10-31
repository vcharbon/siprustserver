/**
 * The summary page's place as the URL carries it: the environment, the tab, the run read and
 * the one compared, the lanes, the filters, the selected facets (checks, groups, families),
 * the open case and the delta on screen. Read from a query string and written back to
 * one, so a reload and a pasted link show the same view.
 */
import type { Campaign, CampaignRun } from "./api.ts"
import type { Filters, Selection } from "./campaign-facets.ts"
import { campaignDelta } from "@sip/viewer-core/routes"
import type { CampaignParams } from "./router.ts"

export interface CampaignState {
  /** The environment asked for; absent, the server's choice. */
  readonly env: string | undefined
  readonly tab: "failures" | "grid"
  readonly run: number | undefined
  readonly vs: number | undefined
  readonly filters: Filters
  readonly selection: Selection
  readonly caseId: string | undefined
  readonly delta: { readonly run: number; readonly lane: string } | undefined
}

const number = (text: string | null): number | undefined => {
  const value = Number(text)
  return text === null || text === "" || !Number.isInteger(value) ? undefined : value
}

export const readState = (query: URLSearchParams): CampaignState => {
  return {
    env: query.get("env") ?? undefined,
    tab: query.get("tab") === "grid" ? "grid" : "failures",
    run: number(query.get("run")),
    vs: number(query.get("vs")),
    filters: {
      lanes: (query.get("lanes") ?? "").split(",").filter((lane) => lane !== ""),
      failing: query.get("fail") === "1",
      oneLane: query.get("one") === "1",
      fresh: query.get("new") === "1",
      text: query.get("q") ?? ""
    },
    selection: { checks: query.getAll("check"), groups: query.getAll("group"), families: query.getAll("family") },
    caseId: query.get("case") ?? undefined,
    delta: campaignDelta(query)
  }
}

const flag = (on: boolean): string | undefined => (on ? "1" : undefined)

export const paramsOf = (state: CampaignState): CampaignParams => ({
  env: state.env,
  tab: state.tab === "grid" ? "grid" : undefined,
  run: state.run === undefined ? undefined : String(state.run),
  vs: state.vs === undefined ? undefined : String(state.vs),
  lanes: state.filters.lanes.length === 0 ? undefined : state.filters.lanes.join(","),
  fail: flag(state.filters.failing),
  one: flag(state.filters.oneLane),
  new: flag(state.filters.fresh),
  q: state.filters.text === "" ? undefined : state.filters.text,
  check: state.selection.checks,
  group: state.selection.groups,
  family: state.selection.families,
  case: state.caseId,
  delta: state.delta === undefined ? undefined : `${state.delta.run}:${state.delta.lane}`
})

/** The run read: the one asked where the view holds it, else the newest. */
export const runOf = (view: Campaign, state: CampaignState): number | undefined =>
  view.runs.find((r) => r.id === state.run)?.id ?? view.runs[0]?.id

/** The cases `run` judged on any lane. */
const judgedCases = (run: CampaignRun | undefined): ReadonlySet<string> =>
  new Set(run?.lanes.flatMap((l) => l.results.filter((r) => r.outcome !== "error").map((r) => r.caseId)) ?? [])

/** The run compared: the one asked, else the latest earlier run that judged a case the one read judged. */
export const vsOf = (view: Campaign, state: CampaignState, run: number | undefined): number | undefined => {
  if (state.vs !== undefined && view.runs.some((r) => r.id === state.vs)) return state.vs
  const read = judgedCases(view.runs.find((r) => r.id === run))
  return view.runs.find((r) => r.id < (run ?? 0) && [...judgedCases(r)].some((caseId) => read.has(caseId)))?.id
}
