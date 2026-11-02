/**
 * A campaign over its runs, oldest first: the run grid (one row per group, a
 * case under each of its groups, under its family where it has none, one cell per run × lane holding the row's worst verdict,
 * failure over error over pass), and one case's history (its verdict per run
 * and lane, the run labels that changed from the run before, and one flip row
 * per lane × check it ever failed, the runs where the verdict changed marked),
 * over the latest runs the case ran in on one of the lanes at least.
 */
import type { Campaign, CampaignCase, CampaignResult } from "./api.ts"
import { checkKey, failedChecks, resultOf } from "./campaign-facets.ts"

export type Verdict = CampaignResult["outcome"] | "none"

const RANK: Readonly<Record<Verdict, number>> = { failure: 3, error: 2, pass: 1, none: 0 }

/** The worse of two verdicts. */
export const worse = (a: Verdict, b: Verdict): Verdict => (RANK[b] > RANK[a] ? b : a)

export interface GridCell {
  readonly run: number
  readonly lane: string
  readonly worst: Verdict
  readonly failing: number
  readonly ran: number
}

export interface GridRow {
  /** The group, or the family of cases in no group. */
  readonly name: string
  readonly cells: readonly GridCell[]
  readonly cases: ReadonlyArray<{ readonly caseId: string; readonly cells: readonly GridCell[] }>
}

export interface Grid {
  /** The run ids, oldest first. */
  readonly runs: readonly number[]
  readonly rows: readonly GridRow[]
}

const chronological = (view: Campaign) => [...view.runs].sort((a, b) => a.id - b.id)

/** The grid rows a case is under: its groups, else its family. */
export const rowsOf = (c: CampaignCase): readonly string[] => (c.groups.length > 0 ? c.groups : [c.family])

export const grid = (view: Campaign, lanes: readonly string[]): Grid => {
  const runs = chronological(view)
  const names = [...new Set(view.cases.flatMap(rowsOf))].sort((a, b) =>
    a.localeCompare(b, undefined, { numeric: true })
  )
  const cellOf = (caseIds: readonly string[], runId: number, lane: string): GridCell => {
    const run = runs.find((r) => r.id === runId)
    let worst: Verdict = "none"
    let failing = 0
    let ran = 0
    for (const caseId of caseIds) {
      const verdict: Verdict = resultOf(run, lane, caseId)?.outcome ?? "none"
      worst = worse(worst, verdict)
      if (verdict !== "none") ran += 1
      if (verdict === "failure" || verdict === "error") failing += 1
    }
    return { run: runId, lane, worst, failing, ran }
  }
  const cellsOf = (caseIds: readonly string[]) =>
    lanes.flatMap((lane) => runs.map((run) => cellOf(caseIds, run.id, lane)))
  return {
    runs: runs.map((r) => r.id),
    rows: names.map((name) => {
      const caseIds = view.cases.filter((c) => rowsOf(c).includes(name)).map((c) => c.caseId)
      return {
        name,
        cells: cellsOf(caseIds),
        cases: caseIds.map((caseId) => ({ caseId, cells: cellsOf([caseId]) }))
      }
    })
  }
}

export interface Flip {
  readonly lane: string
  readonly key: string
  /** Per run: `✓` passed, `✗` failed, `—` not judged. */
  readonly marks: readonly string[]
  /** The runs whose mark differs from the last judged one before. */
  readonly changedAt: readonly number[]
  /** Per run, what was seen where it failed. */
  readonly seen: ReadonlyArray<string | null>
}

export interface History {
  /** The runs shown, oldest first: the latest the case ran in, at most the limit. */
  readonly runs: readonly number[]
  /** The runs the case ran in left out by the limit. */
  readonly hidden: number
  readonly verdicts: Readonly<Record<string, readonly Verdict[]>>
  /** Per run, the labels whose value differs from the shown run before. */
  readonly changed: ReadonlyArray<readonly string[]>
  readonly flips: readonly Flip[]
}

export const history = (
  view: Campaign,
  caseId: string,
  lanes: readonly string[],
  limit: number = Number.POSITIVE_INFINITY
): History => {
  const ran = chronological(view).filter((run) => lanes.some((lane) => resultOf(run, lane, caseId) !== undefined))
  const runs = ran.slice(Math.max(0, ran.length - limit))
  const verdicts: Record<string, Array<Verdict>> = {}
  const keys: Array<{ lane: string; key: string }> = []
  for (const lane of lanes) {
    verdicts[lane] = runs.map((run) => resultOf(run, lane, caseId)?.outcome ?? "none")
    for (const run of runs) {
      const result = resultOf(run, lane, caseId)
      if (result === undefined || result.outcome !== "failure") continue
      for (const check of failedChecks(result)) {
        const key = checkKey(check)
        if (!keys.some((k) => k.lane === lane && k.key === key)) keys.push({ lane, key })
      }
    }
  }
  const changed = runs.map((run, at) => {
    const before = runs[at - 1]
    if (before === undefined) return []
    return run.labels.filter((label) => before.labels.find((l) => l.name === label.name)?.value !== label.value)
      .map((label) => label.name)
  })
  const flips = keys.map(({ lane, key }): Flip => {
    const marks: Array<string> = []
    const seen: Array<string | null> = []
    const changedAt: Array<number> = []
    let last: string | undefined
    for (const run of runs) {
      const result = resultOf(run, lane, caseId)
      const judged = result !== undefined && result.outcome !== "error"
      const failed = judged ? failedChecks(result).find((c) => checkKey(c) === key) : undefined
      const mark = !judged ? "—" : failed === undefined ? "✓" : "✗"
      marks.push(mark)
      seen.push(failed?.seen ?? null)
      if (judged) {
        if (last !== undefined && last !== mark) changedAt.push(run.id)
        last = mark
      }
    }
    return { lane, key, marks, changedAt, seen }
  })
  return { runs: runs.map((r) => r.id), hidden: ran.length - runs.length, verdicts, changed, flips }
}
