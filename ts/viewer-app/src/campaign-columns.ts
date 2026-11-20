/**
 * The cells a campaign cell's records are compared with: the sub-cases of
 * the cell's case on its run and lane (the cases of one ref), or the cell's
 * case on every lane of its run; a column is a cell the run wrote, in the
 * campaign's case order or its lane order, held by its lane run and case.
 */
import type { Campaign } from "./api.ts"

/** One cell a comparison holds: its label, its holder (lane run and case), where its bundle is, its lane. */
export interface CompareColumn {
  readonly label: string
  readonly run: number
  readonly caseId: string
  readonly session: string
  readonly cell: string
  readonly lane: string
}

/** What a comparison holds beside the cell: its case's sub-cases on its lane, or its case on every lane. */
export type CompareAxis = "subcases" | "lanes"

const laneOf = (view: Campaign, run: number, lane: string) =>
  view.runs.find((r) => r.id === run)?.lanes.find((l) => l.lane === lane)

const columnOf = (view: Campaign, run: number, lane: string, caseId: string, label: string): CompareColumn[] => {
  const laneRun = laneOf(view, run, lane)
  const cell = laneRun?.results.find((r) => r.caseId === caseId)?.cell ?? null
  return laneRun === undefined || cell === null
    ? []
    : [{ label, run: laneRun.runId, caseId, session: laneRun.session, cell, lane }]
}

/** The sub-cases of `caseId`'s case on `run` and `lane` that wrote a cell, in the campaign's case order. */
export const subcaseColumns = (view: Campaign, caseId: string, run: number, lane: string): readonly CompareColumn[] => {
  const ref = view.cases.find((c) => c.caseId === caseId)?.ref
  if (ref === undefined) return columnOf(view, run, lane, caseId, caseId)
  return view.cases
    .filter((c) => c.ref === ref)
    .flatMap((c) => columnOf(view, run, lane, c.caseId, c.subcase ?? c.caseId))
}

/** `caseId` on every lane of `run` that wrote it a cell, in the campaign's lane order. */
export const laneColumns = (view: Campaign, caseId: string, run: number): readonly CompareColumn[] =>
  view.lanes.flatMap((lane) => columnOf(view, run, lane, caseId, lane))

/** The columns of `axis` for the cell of `caseId` on `run` and `lane`. */
export const compareColumns = (
  view: Campaign,
  caseId: string,
  run: number,
  lane: string,
  axis: CompareAxis
): readonly CompareColumn[] => (axis === "subcases" ? subcaseColumns(view, caseId, run, lane) : laneColumns(view, caseId, run))
