/**
 * Where a queue row's two links lead. The `capture` cell opens the capture
 * study; the `call` cell opens the study of that one call — a case at the
 * step it died at where the run named one, a refused call at its refusal.
 * Both stay on the lane the queue was read on. The queue's own delta-sheet
 * link carries the slice it reads, whatever row cap it serves.
 */
import type { TriageCallRow } from "./api.ts"
import { type ChipFilter, filterParams } from "./queue-filter.ts"
import { href } from "./router.ts"

export const captureStudyOf = (row: TriageCallRow, lane: string): string =>
  href.triageStudy(row.capture, { lane })

/** The main error's step outranks the verdict's; a call that named none opens on the case. */
export const callStudyOf = (row: TriageCallRow, lane: string): string =>
  href.triageStudy(row.capture, {
    lane,
    case: row.caseId,
    step: row.mainError?.step ?? row.failedStep ?? undefined
  })

/** The delta sheet of the slice the queue reads: its lane, scope, needle and chips; never its row cap. */
export const deltasOf = (
  slice: { readonly lane: string; readonly scope: string; readonly q: string; readonly filter: ChipFilter }
): string => href.deltas({ lane: slice.lane, scope: slice.scope, q: slice.q, ...filterParams(slice.filter) })
