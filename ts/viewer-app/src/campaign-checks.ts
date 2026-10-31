/**
 * What a case checks, as the case details list it: every check the campaign
 * states for the case, in its order, each marked per lane of the newest run
 * that judged the case: failed where a failed check of the lane has its
 * anchor and its field (any field for `*`), passed where the lane judged the
 * case otherwise, none where it did not. A failed check no listed check
 * matches follows the list, so every failure shows.
 */
import type { Campaign, CampaignChecked } from "./api.ts"
import { failedChecks, judgedIn, resultOf } from "./campaign-facets.ts"

export type Mark = "pass" | "failure" | "none"

export interface CheckRow extends CampaignChecked {
  readonly marks: Readonly<Record<string, Mark>>
}

export interface CheckList {
  /** The run the marks are read in; undefined where no run judged the case. */
  readonly run: number | undefined
  readonly rows: readonly CheckRow[]
}

const matches = (row: CampaignChecked, failed: { readonly anchor: string; readonly field: string }): boolean =>
  row.anchor === failed.anchor && (row.field === "*" || row.field === failed.field)

export const checkList = (view: Campaign, caseId: string, lanes: readonly string[]): CheckList => {
  const listed = view.cases.find((c) => c.caseId === caseId)?.checked ?? []
  const run = view.runs.find((r) => judgedIn(r, lanes, caseId))
  const failedOn = (lane: string) => {
    const result = resultOf(run, lane, caseId)
    return result === undefined || result.outcome === "error" ? undefined : failedChecks(result)
  }
  const markOf = (lane: string, row: CampaignChecked): Mark => {
    const failed = failedOn(lane)
    return failed === undefined ? "none" : failed.some((f) => matches(row, f)) ? "failure" : "pass"
  }
  const rows: Array<CheckRow> = listed.map((row) => ({
    ...row,
    marks: Object.fromEntries(lanes.map((lane) => [lane, markOf(lane, row)]))
  }))
  for (const lane of lanes) {
    for (const failed of failedOn(lane) ?? []) {
      if (listed.some((row) => matches(row, failed))) continue
      const known = rows.find((row) => row.anchor === failed.anchor && row.field === failed.field && row.op === "")
      if (known !== undefined) {
        ;(known.marks as Record<string, Mark>)[lane] = "failure"
        continue
      }
      rows.push({
        anchor: failed.anchor,
        field: failed.field,
        op: "",
        expected: failed.expected,
        marks: Object.fromEntries(lanes.map((l) => [l, l === lane ? "failure" : failedOn(l) === undefined ? "none" : "pass"]))
      })
    }
  }
  return { run: run?.id, rows }
}
