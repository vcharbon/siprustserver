/**
 * Every case id and run a ticket cites, read back through the resolver: one
 * JSON line per citation — the id as cited, its ticket and day, and what it
 * resolves to (status, capture, case id, call key) — and one count per status.
 * Two reports of one store, taken before and after a wipe, say line by line
 * whether every cited id still names the same call.
 */
import { type Citations, Resolver } from "@sip/store"
import * as Effect from "effect/Effect"

/** One cited id, as the report states it. */
export interface Line {
  readonly cited: string
  readonly kind: Citations.Kind
  readonly by: string
  readonly on: string | null
  readonly status: Resolver.Status
  readonly capture: string | null
  readonly caseId: string | null
  readonly callKey: string | null
  readonly total: number
}

/** The kinds a report reads back: the ones a resolver answers for. */
const READ: ReadonlySet<Citations.Kind> = new Set(["case", "run"])

/** Every citation of a case or a run, resolved; one line each, in citation order. */
export const report = Effect.fn("CitedReport.report")(function*(citations: ReadonlyArray<Citations.Citation>) {
  const resolver = yield* Resolver.Service
  const lines: Array<Line> = []
  for (const citation of citations) {
    if (!READ.has(citation.kind)) continue
    const input = citation.kind === "run" ? `run${citation.ref}` : citation.ref
    const answer = yield* resolver.resolve(
      input,
      citation.citedOn === null ? {} : { citedOn: citation.citedOn }
    )
    const call = answer.status === "resolved" || answer.status === "refused" ? answer.calls[0] : undefined
    lines.push({
      cited: input,
      kind: citation.kind,
      by: citation.citedBy,
      on: citation.citedOn,
      status: answer.status,
      capture: call?.capture ?? null,
      caseId: call?.caseId ?? null,
      callKey: call?.callKey ?? null,
      total: answer.total
    })
  }
  return lines as ReadonlyArray<Line>
})

/** How many lines each (kind, status) holds, as one line per pair. */
export const tally = (lines: ReadonlyArray<Line>): ReadonlyArray<string> => {
  const counts = new Map<string, number>()
  for (const line of lines) {
    const key = `${line.kind} ${line.status}`
    counts.set(key, (counts.get(key) ?? 0) + 1)
  }
  return [...counts.entries()].sort(([a], [b]) => (a < b ? -1 : 1)).map(([key, n]) => `[cited] ${key}: ${n}`)
}
