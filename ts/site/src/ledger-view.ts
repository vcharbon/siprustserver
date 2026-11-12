/**
 * A case's ledger row — what the backend mock answers for the case, as the
 * cut wrote it (`ledger/<number>.json`: the number dialled, the number the
 * capture showed, one entry per answer family with the request it matches) —
 * read into the viewer's generic `MockAnswers`: the target, what the capture
 * observed, and one row per answer with its label, its match keys on one line
 * and the answer verbatim. Nothing is derived: a half no entry carries is a
 * half the mock does not send.
 */

/** One answer the mock serves: its family, the request it matches, the answer verbatim. */
export interface MockAnswer {
  readonly label: string
  readonly match: string
  readonly answer: unknown
}

/** What the lane's mock answers for one case; the shape the app's panel draws. */
export interface MockAnswers {
  readonly target: string | null
  readonly observed: string | null
  readonly answers: ReadonlyArray<MockAnswer>
}

const asRecord = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : undefined

const text = (value: unknown): string | null => (typeof value === "string" && value !== "" ? value : null)

/** The match keys on one line — an entry answers a call, not a page of prose. */
const matchLine = (match: unknown): string => {
  const keys = asRecord(match)
  return keys === undefined
    ? ""
    : Object.entries(keys).map(([key, value]) => `${key}=${String(value)}`).join(" ")
}

/** The row as the viewer's mock answers; null where there is no row. */
export const mockAnswersOf = (row: unknown): MockAnswers | null => {
  const read = asRecord(row)
  if (read === undefined) return null
  const entries = Array.isArray(read["entries"]) ? read["entries"] : []
  return {
    target: text(read["dialled"]) ?? text(read["number"]),
    observed: text(read["observed_dialled"]),
    answers: entries.map((entry) => {
      const one = asRecord(entry) ?? {}
      return { label: text(one["family"]) ?? "?", match: matchLine(one["match"]), answer: one["answer"] ?? null }
    })
  }
}
