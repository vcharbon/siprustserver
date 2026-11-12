/**
 * The holder an attachment list is asked for, as a query names it: a cell by
 * its `run` and `case`, a `batch`, or a `job`, exactly one of the three; any
 * other query names none.
 */
import type { Attachments } from "@sip/store"

/** The query forms a holder is named in. */
export const HOLDER_FORMS = "run=<id>&case=<case id> | batch=<id> | job=<id>"

/** The holder `query` names; undefined where it names none, or more than one. */
export const holderOf = (query: {
  readonly run?: number | undefined
  readonly case?: string | undefined
  readonly batch?: number | undefined
  readonly job?: number | undefined
}): Attachments.Holder | undefined => {
  const cell = query.run !== undefined || query.case !== undefined
  const named = [cell, query.batch !== undefined, query.job !== undefined].filter(Boolean).length
  if (named !== 1) return undefined
  if (cell) {
    return query.run === undefined || query.case === undefined || query.case === ""
      ? undefined
      : { scope: "cell", runId: query.run, caseId: query.case }
  }
  return query.batch !== undefined ? { scope: "batch", batchId: query.batch } : { scope: "job", jobId: query.job! }
}
