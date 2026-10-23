/**
 * The one line under a study's title: how many calls the capture holds and how
 * the run ruled on each. The refused calls are the exclusion records the cut
 * wrote; the rest are the standing run's cases, green, red or skipped. A
 * capture never run holds its refusals and, where a cut stands, that cut's
 * cases, none of them ruled on.
 */
import type { ExclusionRecord } from "@sip/viewer-core"

export interface RunCounts {
  /** The run's cases and the cut's refusals together. */
  readonly calls: number
  readonly refused: number
  readonly green: number
  readonly red: number
  readonly skipped: number
}

/** What a standing run says of its cases. */
export interface CountedStanding {
  readonly cases: number
  readonly failed: number
  readonly caseResults: ReadonlyArray<{ readonly skipped: string | null }>
}

export const runCounts = (
  standing: CountedStanding | undefined,
  exclusions: ReadonlyArray<ExclusionRecord> | undefined,
  /** The standing cut's case count: what a capture never run holds. */
  cutCases?: number
): RunCounts => {
  const refused = exclusions?.length ?? 0
  const calls = (standing?.cases ?? cutCases ?? 0) + refused
  if (standing === undefined) return { calls, refused, green: 0, red: 0, skipped: 0 }
  const red = standing.failed
  const skipped = standing.caseResults.filter((result) => result.skipped !== null).length
  return { calls, refused, green: standing.cases - red - skipped, red, skipped }
}
