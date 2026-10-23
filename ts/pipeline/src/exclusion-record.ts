/**
 * One refused case ON DISK: the record a cut writes to `excluded.json` beside
 * the cases it did generate, one per refusing verdict. A viewer reads
 * `caseId`, `reason`, `callIds`, `legs`, `sut`, `groups`, `evidence` and
 * `line` off these records, so the key set and their order are load-bearing.
 *
 * `legs` and `callIds` are the CASE's — every vantage the cut gave it — and
 * `sut` the addresses the cut decided the SUT at, so a reader with no document
 * of the case still draws the whole call, oriented. The coordinates the rule
 * CHARGED are the `evidence`'s own, never restated here under another key.
 */
import type { Refusal, RefusalEvidence } from "./policy.js"

export interface ExclusionRecord {
  readonly caseId: string
  readonly reason: string
  readonly groups?: ReadonlyArray<number>
  readonly legs: ReadonlyArray<number>
  readonly callIds: ReadonlyArray<string>
  readonly sut: ReadonlyArray<string>
  readonly evidence?: ReadonlyArray<RefusalEvidence>
  readonly line: string
  /** The call the refused case is of, which freezes its id; absent where the cut named none. */
  readonly callKey?: string
}

/** The case a refusal is recorded against: its legs, its calls, the SUT the cut decided, and its call key. */
export interface RefusedCase {
  readonly legs: ReadonlyArray<number>
  readonly callIds: ReadonlyArray<string>
  readonly sut: ReadonlyArray<string>
  readonly callKey?: string
}

/** The legs of every set, ascending and deduplicated. */
const legsUnion = (...sets: ReadonlyArray<ReadonlyArray<number>>): ReadonlyArray<number> =>
  [...new Set(sets.flat())].sort((a, b) => a - b)

/**
 * The on-disk record of one refusal against the case it was decided on. A
 * leg or call the rule names beyond the case's own is kept — a cut-tier
 * refusal charges a family no case was ever proposed for — so the record's
 * legs and calls are never narrower than either.
 */
export const exclusionRecordOf = (refusal: Refusal, on: RefusedCase): ExclusionRecord => ({
  caseId: refusal.caseId,
  reason: refusal.reason,
  ...(refusal.groups === undefined ? {} : { groups: refusal.groups }),
  legs: legsUnion(on.legs, refusal.legs ?? []),
  callIds: [...new Set([...on.callIds, ...(refusal.callIds ?? [])])],
  sut: on.sut,
  ...(refusal.evidence === undefined ? {} : { evidence: refusal.evidence }),
  line: refusal.line,
  ...(on.callKey === undefined ? {} : { callKey: on.callKey })
})
