/**
 * `Calls.queue`, in memory: the queue statement's contract stated over rows,
 * so a suite with no store serves it from a fixture and the store suite reads
 * the same words. The statement is the contract; this is its plain reading.
 *
 * The POOL is the rows the scope and the capture substring keep, and the
 * groups are counted over it. KEPT is the pool minus the excludes: what the
 * distinct captures count. MATCHED is kept plus the includes: what the page
 * is cut from, cases first, then by capture and case id.
 */
import type { CallRow, Queue } from "./calls.js"
import type { Excludes, Group, Outcome, Query, Scope } from "./facets.js"
import { DEFAULT_SCOPE, GROUPS, rulesOf } from "./facets.js"

/** The row kind the queue shows: refused, else skipped, else passed, else failed. */
export const outcomeOf = (row: CallRow): Outcome =>
  row.kind === "refused" ? "refused" : row.skipped !== null ? "skipped" : row.passed ? "passed" : "failed"

const inScope = (row: CallRow, scope: Scope): boolean => {
  const outcome = outcomeOf(row)
  switch (scope) {
    case "all":
      return true
    case "red":
      return outcome === "failed" || outcome === "refused"
    default:
      return outcome === scope
  }
}

const blank = (value: string | undefined): boolean => value === undefined || value === ""

const ruleOf = (row: CallRow): string | null => (row.kind === "case" ? row.mainError?.rule ?? null : null)
const expectedOf = (row: CallRow): string | null => (row.kind === "case" ? row.mainError?.expected ?? null : null)
const refusalsOf = (row: CallRow): ReadonlyArray<string> => (row.kind === "refused" ? row.reasons : [])

const one = (valueOf: (row: CallRow) => string | null) => (row: CallRow): ReadonlyArray<string> => {
  const value = valueOf(row)
  return value === null ? [] : [value]
}

const badBodyOf = (row: CallRow): boolean => row.kind === "case" && (row.bodyFailing ?? 0) > 0

const sourceOf = (row: CallRow): string | null => (row.kind === "case" ? row.sourceFault?.rule ?? null : null)

const valuesOf = {
  rule: (row: CallRow): ReadonlyArray<string> => rulesOf(ruleOf(row), badBodyOf(row)),
  expected: one(expectedOf),
  refusal: refusalsOf,
  outcome: one(outcomeOf),
  feature: (row: CallRow): ReadonlyArray<string> => row.features,
  source: one(sourceOf)
} as const

const excluded = (row: CallRow, exclude: Excludes): boolean =>
  GROUPS.some((group) => {
    const hidden = exclude[group]
    return hidden !== undefined && hidden.length > 0 && valuesOf[group](row).some((value) => hidden.includes(value))
  })

const included = (row: CallRow, query: Query): boolean =>
  GROUPS.every((group) => blank(query[group]) || valuesOf[group](row).includes(query[group] ?? ""))

const text = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0)

/** Cases before refused calls, then capture, then case id: the page's order. */
export const byCall = (a: CallRow, b: CallRow): number =>
  text(a.kind, b.kind) || text(a.capture, b.capture) || text(a.caseId, b.caseId)

/** The rows the scope and the capture substring keep. */
export const pool = (rows: ReadonlyArray<CallRow>, query: Query): ReadonlyArray<CallRow> => {
  const scope = query.scope ?? DEFAULT_SCOPE
  const capture = blank(query.capture) ? undefined : query.capture?.toLowerCase()
  return rows.filter((row) =>
    inScope(row, scope) && (capture === undefined || row.capture.toLowerCase().includes(capture))
  )
}

/** The group a row answers under: its outcome, its main error's facets and its source fault, a refused row's reasons. */
export const groupOf = (row: CallRow): Omit<Group, "count"> => ({
  kind: row.kind,
  outcome: outcomeOf(row),
  errorKind: row.mainError?.kind ?? null,
  rule: ruleOf(row),
  rfc: row.kind === "case" ? row.mainError?.rfc ?? null : null,
  expected: expectedOf(row),
  reasons: refusalsOf(row),
  features: row.features,
  ...(row.kind === "case" && row.sourceFault !== undefined ? { source: row.sourceFault.rule } : {}),
  ...(badBodyOf(row) ? { badBody: true as const } : {})
})

/** One group per distinct answer, in first-seen order, each counting the rows giving it. */
export const groupsOf = (rows: ReadonlyArray<CallRow>): ReadonlyArray<Group> => {
  const groups = new Map<string, Group>()
  for (const row of rows) {
    const group = groupOf(row)
    const key = JSON.stringify(group)
    const seen = groups.get(key)
    groups.set(key, seen === undefined ? { ...group, count: 1 } : { ...seen, count: seen.count + 1 })
  }
  return [...groups.values()]
}

/** What `Calls.queue(lane, query, limit)` answers for a lane whose rows are these; `limit` 0 = no limit. */
export const queueOf = (rows: ReadonlyArray<CallRow>, query: Query, limit: number): Queue => {
  const pooled = pool(rows, query)
  const exclude = query.exclude ?? {}
  const kept = pooled.filter((row) => !excluded(row, exclude))
  const matched = kept.filter((row) => included(row, query)).sort(byCall)
  return {
    rows: limit > 0 ? matched.slice(0, limit) : matched,
    matched: matched.length,
    captures: new Set(kept.map((row) => row.capture)).size,
    groups: groupsOf(pooled)
  }
}
