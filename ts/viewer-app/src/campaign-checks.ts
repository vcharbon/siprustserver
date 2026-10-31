/**
 * What a case checks, as the case details list it: every check the newest run
 * that judged the case states for it, in its order, each marked per lane of
 * that run: failed where a failed check of the lane is its own, passed where
 * the lane judged the case otherwise, none where it did not.
 *
 * A failed check is the listed check's own where both name one flow step: of
 * its field (the one whose expected value it states, where several share the
 * field), else the step's `*`. Without a step, a listed check of no step at its
 * anchor takes it, by field or `*`. A failed check no listed one takes, and a
 * lane that failed without naming a check, follow the list, so every failure
 * shows.
 */
import type { Campaign, CampaignCheck, CampaignChecked } from "./api.ts"
import { failedChecks, judgedIn, resultOf } from "./campaign-facets.ts"

export type Mark = "pass" | "failure" | "none"

export interface CheckRow extends CampaignChecked {
  readonly marks: Readonly<Record<string, Mark>>
}

export interface CheckList {
  /** The run the list and its marks are read in; undefined where no run judged the case. */
  readonly run: number | undefined
  readonly rows: readonly CheckRow[]
}

/** The index of the listed check `failed` is the failure of; -1 where none is. */
export const ownerOf = (listed: readonly CampaignChecked[], failed: CampaignCheck): number => {
  const step = failed.step ?? null
  if (step !== null) {
    const same = listed.flatMap((row, at) => (row.step === step && row.field === failed.field ? [at] : []))
    if (same.length > 0) return same.find((at) => listed[at]!.expected === failed.expected) ?? same[0]!
    return listed.findIndex((row) => row.step === step && row.field === "*")
  }
  const exact = listed.findIndex((row) => row.step === null && row.anchor === failed.anchor && row.field === failed.field)
  return exact >= 0 ? exact : listed.findIndex((row) => row.step === null && row.anchor === failed.anchor && row.field === "*")
}

/** The row a lane that failed without naming a check is marked on. */
const UNNAMED: CampaignChecked = { step: null, anchor: "call", field: "verdict", op: "", expected: null }

export const checkList = (view: Campaign, caseId: string, lanes: readonly string[]): CheckList => {
  const run = view.runs.find((r) => judgedIn(r, lanes, caseId))
  const listed = run?.checked[caseId] ?? []
  const judged = new Set<string>()
  const failedAt = new Map<number, Set<string>>()
  const extra: Array<{ readonly check: CampaignChecked; readonly lanes: Set<string> }> = []
  const addExtra = (check: CampaignChecked, lane: string) => {
    const known = extra.find((e) => e.check.anchor === check.anchor && e.check.field === check.field && e.check.step === check.step)
    if (known !== undefined) known.lanes.add(lane)
    else extra.push({ check, lanes: new Set([lane]) })
  }
  for (const lane of lanes) {
    const result = resultOf(run, lane, caseId)
    if (result === undefined || result.outcome === "error") continue
    judged.add(lane)
    const failed = failedChecks(result)
    if (result.outcome === "failure" && failed.length === 0) addExtra(UNNAMED, lane)
    for (const check of failed) {
      const at = ownerOf(listed, check)
      if (at >= 0) failedAt.set(at, (failedAt.get(at) ?? new Set()).add(lane))
      else addExtra({ step: check.step ?? null, anchor: check.anchor, field: check.field, op: "", expected: check.expected }, lane)
    }
  }
  const markOf = (lane: string, failed: ReadonlySet<string> | undefined): Mark =>
    !judged.has(lane) ? "none" : failed?.has(lane) === true ? "failure" : "pass"
  return {
    run: run?.id,
    rows: [
      ...listed.map((row, at) => ({ ...row, marks: Object.fromEntries(lanes.map((lane) => [lane, markOf(lane, failedAt.get(at))])) })),
      ...extra.map(({ check, lanes: on }) => ({ ...check, marks: Object.fromEntries(lanes.map((lane) => [lane, markOf(lane, on)])) }))
    ]
  }
}
