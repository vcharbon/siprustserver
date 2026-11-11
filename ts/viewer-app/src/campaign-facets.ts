/**
 * The failures of one campaign run as facets. Every failed check (anchor and
 * field) is a facet and all weigh the same: a call counts under each check it
 * fails, so the counts overlap. A cell no call was judged in is the facet
 * `error <its first line>`. Groups and families stand beside the checks; a
 * call counts under each of its groups too. Selected checks narrow the calls
 * by AND, selected groups and families each by OR, the three by AND; one
 * selected check is read by the shapes of what was seen (long digit runs and
 * ports folded), so many failures of one cause read as one.
 *
 * Each call's status and verification lines on the lanes read are facets too
 * (a line by its name, answer and mark, `call fail waived`): a call counts
 * under each status and line it holds on a lane read, passing calls
 * included. Selected statuses narrow by OR, selected lines by AND; with
 * either selected, the calls listed are every call they keep, not only the
 * failing ones.
 *
 * A check is dated from the first run of its current streak: the oldest run,
 * going back without a gap, in which some call failed it on the lanes it fails
 * on now; a run that judged no call on those lanes neither breaks nor extends
 * the streak. A check passes
 * again when the compared run failed it and no call of this run does.
 */
import type { Campaign, CampaignCheck, CampaignResult, CampaignRun, CellStatus } from "./api.ts"
import { lineKey, STATUSES } from "./verification-lines.ts"

/** The page's filters: the lanes read, and the case filters. */
export interface Filters {
  /** The lanes read; empty reads every lane. */
  readonly lanes: readonly string[]
  /** Only calls failing on at least one lane read. */
  readonly failing: boolean
  /** Only calls failing on exactly one lane read. */
  readonly oneLane: boolean
  /** Only calls failing on a lane where the compared run did not fail them. */
  readonly fresh: boolean
  readonly text: string
}

export const ALL_FILTERS: Filters = { lanes: [], failing: false, oneLane: false, fresh: false, text: "" }

export interface Selection {
  readonly checks: readonly string[]
  readonly groups: readonly string[]
  readonly families: readonly string[]
  readonly statuses: readonly string[]
  /** Lines by {@link lineKey}. */
  readonly lines: readonly string[]
}

export const NO_SELECTION: Selection = { checks: [], groups: [], families: [], statuses: [], lines: [] }

/** A kind of facet the page selects. */
export type FacetKind = "check" | "group" | "family" | "status" | "line"

/** The selection field of each facet kind. */
export const SELECTED: Readonly<Record<FacetKind, keyof Selection>> = {
  check: "checks",
  group: "groups",
  family: "families",
  status: "statuses",
  line: "lines"
}

/** A result's status: the one the server states, else read off its outcome. */
export const statusOf = (result: CampaignResult): CellStatus =>
  result.status ?? (result.outcome === "pass" ? "pass" : result.outcome === "failure" ? "fail" : "error")

export interface CheckFacet {
  readonly key: string
  readonly anchor: string
  readonly field: string
  /** The calls failing it. */
  readonly calls: number
  /** Per lane, the calls failing it there. */
  readonly lanes: Readonly<Record<string, number>>
  /** Per side, the call·lanes failing it so. */
  readonly sides: Readonly<Record<string, number>>
  /** The run its streak began in. */
  readonly since: number
}

export interface Shape {
  readonly shape: string
  readonly side: string
  readonly count: number
  /** One check of the shape, as stated. */
  readonly sample: CampaignCheck
}

export interface Match {
  readonly caseId: string
  readonly title: string | null
  readonly groups: readonly string[]
  readonly family: string
  /** Each lane read: the call's outcome there, `none` where it did not run. */
  readonly outcomes: Readonly<Record<string, CampaignResult["outcome"] | "none">>
  /** Each lane read: the call's status there, `none` where it did not run. */
  readonly statuses: Readonly<Record<string, CellStatus | "none">>
  /** Every check it failed, with the lanes it failed it on. */
  readonly failed: ReadonlyArray<{ readonly key: string; readonly lanes: readonly string[] }>
}

export interface Facets {
  readonly checks: readonly CheckFacet[]
  readonly groups: ReadonlyArray<{ readonly group: string; readonly calls: number }>
  readonly families: ReadonlyArray<{ readonly family: string; readonly calls: number }>
  readonly passingAgain: ReadonlyArray<{ readonly key: string; readonly calls: number }>
  /** The statuses the calls hold on the lanes read, in the pages' order. */
  readonly statuses: ReadonlyArray<{ readonly status: CellStatus; readonly calls: number }>
  /** The lines the calls hold on the lanes read, by key; the most held first. */
  readonly lines: ReadonlyArray<{ readonly key: string; readonly calls: number }>
  /** The selected check's shapes, where exactly one check is selected. */
  readonly shapes: readonly Shape[]
  readonly matching: readonly Match[]
  /** Calls of the run on the lanes read, filters held. */
  readonly ran: number
  /** Of those, the ones failing somewhere. */
  readonly failing: number
}

export const checkKey = (check: { readonly anchor: string; readonly field: string }): string =>
  `${check.anchor} ${check.field}`

/** A run of six digits or more, and a port after an IPv4 address, folded to `#`: what differs per call, one shape. */
export const shapeOf = (seen: string | null): string =>
  seen === null ? "(absent)" : seen.replace(/(\d{1,3}(?:\.\d{1,3}){3}):\d+/g, "$1:#").replace(/\d{6,}/g, "#")

/** The failed checks of a result, an error's included as its one pseudo-check. */
export const failedChecks = (result: CampaignResult): readonly CampaignCheck[] =>
  result.outcome === "error"
    ? [{
      anchor: "error",
      field: (result.detail ?? "no call judged").split("\n")[0]!,
      expected: null,
      seen: result.detail,
      said: null,
      side: "error"
    }]
    : result.checks

/** The lanes `filters` reads. */
export const lanesRead = (view: Campaign, filters: Filters): readonly string[] =>
  filters.lanes.length === 0 ? view.lanes : view.lanes.filter((lane) => filters.lanes.includes(lane))

/** The result of `caseId` on `lane` in `run`; undefined where it did not run. */
export const resultOf = (run: CampaignRun | undefined, lane: string, caseId: string): CampaignResult | undefined =>
  run?.lanes.find((l) => l.lane === lane)?.results.find((r) => r.caseId === caseId)

const failing = (result: CampaignResult | undefined): boolean =>
  result !== undefined && result.outcome !== "pass"

/** The calls each check some call of `run` failed on `lanes`, by key. */
const keysOf = (run: CampaignRun | undefined, lanes: readonly string[]): ReadonlyMap<string, ReadonlySet<string>> => {
  const calls = new Map<string, Set<string>>()
  for (const lane of run?.lanes ?? []) {
    if (!lanes.includes(lane.lane)) continue
    for (const result of lane.results) {
      for (const check of failedChecks(result)) {
        const key = checkKey(check)
        calls.set(key, (calls.get(key) ?? new Set()).add(result.caseId))
      }
    }
  }
  return calls
}

/** Whether `run` judged a call of `caseId` on one of `lanes`. */
export const judgedIn = (run: CampaignRun | undefined, lanes: readonly string[], caseId: string): boolean =>
  lanes.some((lane) => {
    const outcome = resultOf(run, lane, caseId)?.outcome
    return outcome === "pass" || outcome === "failure"
  })

const compare = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0)

export const facets = (
  view: Campaign,
  runId: number,
  vsId: number | undefined,
  filters: Filters,
  selection: Selection
): Facets => {
  const lanes = lanesRead(view, filters)
  const order = [...view.runs].sort((a, b) => a.id - b.id)
  const run = view.runs.find((r) => r.id === runId)
  const vs = vsId === undefined ? undefined : view.runs.find((r) => r.id === vsId)
  const text = filters.text.trim().toLowerCase()

  const kept = view.cases.filter((c) => {
    const now = lanes.map((lane) => resultOf(run, lane, c.caseId))
    if (now.every((r) => r === undefined)) return false
    const bad = now.filter(failing).length
    if (filters.failing && bad === 0) return false
    if (filters.oneLane && bad !== 1) return false
    if (
      filters.fresh &&
      !lanes.some((lane) => failing(resultOf(run, lane, c.caseId)) && resultOf(vs, lane, c.caseId)?.outcome === "pass")
    ) {
      return false
    }
    if (text !== "") {
      const keys = now.flatMap((r) => (r === undefined ? [] : failedChecks(r).map(checkKey)))
      const about = [c.title ?? "", ...c.groups].join(" ")
      if (!`${c.caseId} ${about} ${keys.join(" ")}`.toLowerCase().includes(text)) return false
    }
    return true
  })

  const byKey = new Map<string, {
    anchor: string
    field: string
    calls: Set<string>
    lanes: Record<string, number>
    sides: Record<string, number>
    shapes: Map<string, Shape>
  }>()
  const families = new Map<string, number>()
  const groups = new Map<string, number>()
  const statusCalls = new Map<CellStatus, number>()
  const lineCalls = new Map<string, number>()
  const matches: Array<Match> = []
  const narrowing = selection.statuses.length > 0 || selection.lines.length > 0
  let failingCalls = 0
  for (const c of kept) {
    const failed = new Map<string, Array<string>>()
    const outcomes: Record<string, CampaignResult["outcome"] | "none"> = {}
    const statusOn: Record<string, CellStatus | "none"> = {}
    const statuses = new Set<CellStatus>()
    const lineKeys = new Set<string>()
    for (const lane of lanes) {
      const result = resultOf(run, lane, c.caseId)
      outcomes[lane] = result?.outcome ?? "none"
      statusOn[lane] = result === undefined ? "none" : statusOf(result)
      if (result === undefined) continue
      statuses.add(statusOf(result))
      for (const line of result.lines ?? []) lineKeys.add(lineKey(line))
      for (const check of failedChecks(result)) {
        const key = checkKey(check)
        const on = failed.get(key) ?? []
        if (!on.includes(lane)) on.push(lane)
        failed.set(key, on)
        const facet = byKey.get(key) ?? {
          anchor: check.anchor,
          field: check.field,
          calls: new Set<string>(),
          lanes: {},
          sides: {},
          shapes: new Map<string, Shape>()
        }
        facet.calls.add(c.caseId)
        facet.lanes[lane] = (facet.lanes[lane] ?? 0) + 1
        const side = check.side ?? "untyped"
        facet.sides[side] = (facet.sides[side] ?? 0) + 1
        const shape = shapeOf(check.seen)
        const at = `${shape}\u0000${side}`
        const known = facet.shapes.get(at)
        facet.shapes.set(at, { shape, side, count: (known?.count ?? 0) + 1, sample: known?.sample ?? check })
        byKey.set(key, facet)
      }
    }
    for (const status of statuses) statusCalls.set(status, (statusCalls.get(status) ?? 0) + 1)
    for (const key of lineKeys) lineCalls.set(key, (lineCalls.get(key) ?? 0) + 1)
    if (failed.size > 0) {
      failingCalls += 1
      families.set(c.family, (families.get(c.family) ?? 0) + 1)
      for (const group of c.groups) groups.set(group, (groups.get(group) ?? 0) + 1)
    }
    if (failed.size === 0 && !narrowing) continue
    if (selection.statuses.length > 0 && !selection.statuses.some((status) => statuses.has(status as CellStatus))) continue
    if (!selection.lines.every((key) => lineKeys.has(key))) continue
    const keys = [...failed.keys()]
    if (!selection.checks.every((key) => keys.includes(key))) continue
    if (selection.groups.length > 0 && !selection.groups.some((group) => c.groups.includes(group))) continue
    if (selection.families.length > 0 && !selection.families.includes(c.family)) continue
    matches.push({
      caseId: c.caseId,
      title: c.title,
      groups: c.groups,
      family: c.family,
      outcomes,
      statuses: statusOn,
      failed: keys.map((key) => ({ key, lanes: failed.get(key)! }))
    })
  }

  const now = keysOf(run, lanes)
  const before = keysOf(vs, lanes)

  const since = (key: string, on: readonly string[]): number => {
    const failingNow = [...(keysOf(run, on).get(key) ?? [])]
    let first = runId
    for (let at = order.findIndex((r) => r.id === runId) - 1; at >= 0; at -= 1) {
      const earlier = order[at]!
      if (keysOf(earlier, on).has(key)) {
        first = earlier.id
        continue
      }
      if (failingNow.some((caseId) => judgedIn(earlier, on, caseId))) break
    }
    return first
  }
  const checks = [...byKey.entries()]
    .map(([key, f]): CheckFacet => ({
      key,
      anchor: f.anchor,
      field: f.field,
      calls: f.calls.size,
      lanes: f.lanes,
      sides: f.sides,
      since: since(key, Object.keys(f.lanes))
    }))
    .sort((a, b) => b.calls - a.calls || compare(a.key, b.key))
  const single = selection.checks.length === 1 ? byKey.get(selection.checks[0]!) : undefined
  return {
    checks,
    groups: [...groups].map(([group, calls]) => ({ group, calls }))
      .sort((a, b) => b.calls - a.calls || compare(a.group, b.group)),
    families: [...families].map(([family, calls]) => ({ family, calls }))
      .sort((a, b) => b.calls - a.calls || compare(a.family, b.family)),
    passingAgain: [...before].flatMap(([key, calls]) => {
      const judgedNow = [...calls].filter((caseId) => judgedIn(run, lanes, caseId))
      return now.has(key) || judgedNow.length === 0 ? [] : [{ key, calls: judgedNow.length }]
    }).sort((a, b) => compare(a.key, b.key)),
    statuses: STATUSES.flatMap((status) => {
      const calls = statusCalls.get(status)
      return calls === undefined ? [] : [{ status, calls }]
    }),
    lines: [...lineCalls].map(([key, calls]) => ({ key, calls })).sort((a, b) => b.calls - a.calls || compare(a.key, b.key)),
    shapes: single === undefined ? [] : [...single.shapes.values()].sort((a, b) => b.count - a.count),
    matching: matches,
    ran: kept.length,
    failing: failingCalls
  }
}
