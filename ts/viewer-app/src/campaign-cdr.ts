/**
 * A campaign cell's CDRs as its call details read them: each call's declared
 * raw CDR lines beside the lines seen for it, and the cells a CDR delta
 * compares the cell with.
 *
 * A delta compares the sub-cases of the cell's case on its run and lane (the
 * cases of one ref), or the cell's case on every lane of its run; a column is
 * a cell the run wrote, in the campaign's case order or its lane order.
 */
import type { Campaign, CampaignDeclaredLines, CellCdr } from "./api.ts"

/** One cell a delta compares: its label, where its bundle is, and what it is. */
export interface DeltaColumn {
  readonly label: string
  readonly session: string
  readonly cell: string
  readonly caseId: string
  readonly lane: string
}

/** What a delta compares the cell with: its case's sub-cases on its lane, or its case on every lane. */
export type DeltaAxis = "subcases" | "lanes"

/** One call of the cell: its declared lines (null where none is declared) beside the lines seen for it (null where the cell holds none). */
export interface CallLines {
  /** The call's id in the case; null for seen lines no declared call names. */
  readonly call: string | null
  readonly leg: string
  readonly callId: string | null
  readonly declared: number | null
  readonly seen: number | null
}

const laneOf = (view: Campaign, run: number, lane: string) =>
  view.runs.find((r) => r.id === run)?.lanes.find((l) => l.lane === lane)

const columnOf = (view: Campaign, run: number, lane: string, caseId: string, label: string): DeltaColumn[] => {
  const laneRun = laneOf(view, run, lane)
  const cell = laneRun?.results.find((r) => r.caseId === caseId)?.cell ?? null
  return laneRun === undefined || cell === null ? [] : [{ label, session: laneRun.session, cell, caseId, lane }]
}

/** The sub-cases of `caseId`'s case on `run` and `lane` that wrote a cell, in the campaign's case order. */
export const subcaseColumns = (view: Campaign, caseId: string, run: number, lane: string): readonly DeltaColumn[] => {
  const ref = view.cases.find((c) => c.caseId === caseId)?.ref
  if (ref === undefined) return columnOf(view, run, lane, caseId, caseId)
  return view.cases
    .filter((c) => c.ref === ref)
    .flatMap((c) => columnOf(view, run, lane, c.caseId, c.subcase ?? c.caseId))
}

/** `caseId` on every lane of `run` that wrote it a cell, in the campaign's lane order. */
export const laneColumns = (view: Campaign, caseId: string, run: number): readonly DeltaColumn[] =>
  view.lanes.flatMap((lane) => columnOf(view, run, lane, caseId, lane))

/** The columns of `axis` for the cell of `caseId` on `run` and `lane`. */
export const deltaColumns = (
  view: Campaign,
  caseId: string,
  run: number,
  lane: string,
  axis: DeltaAxis
): readonly DeltaColumn[] => (axis === "subcases" ? subcaseColumns(view, caseId, run, lane) : laneColumns(view, caseId, run))

/** The raw CDR lines `run` declares for each call of `caseId`; undefined where it states none. */
export const declaredOf = (view: Campaign, caseId: string, run: number): readonly CampaignDeclaredLines[] | undefined =>
  view.runs.find((r) => r.id === run)?.rawCdr?.[caseId]

/**
 * Each declared call beside the lines seen for the Call-ID its caller leg
 * mints (`<leg>-…`), then the seen calls no declared one names; every count
 * seen null where the cell holds no lines.
 */
export const callLines = (
  declared: readonly CampaignDeclaredLines[] | undefined,
  seen: CellCdr["lines"]
): readonly CallLines[] => {
  const listed = (declared ?? []).map((d) => {
    const lines = seen?.find((s) => s.callId.startsWith(`${d.leg}-`))
    return { call: d.call, leg: d.leg, callId: lines?.callId ?? null, declared: d.lines, seen: seen === null ? null : lines?.lines ?? 0 }
  })
  const named = new Set(listed.flatMap((c) => (c.callId === null ? [] : [c.callId])))
  const rest = (seen ?? [])
    .filter((s) => !named.has(s.callId))
    .map((s) => ({ call: null, leg: s.leg, callId: s.callId, declared: null, seen: s.lines }))
  return [...listed, ...rest]
}
