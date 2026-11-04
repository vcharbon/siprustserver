/**
 * The attempts of a cell replayed more than once on its lane, as the case
 * history lists them: one line each, in order, its outcome, its failed checks
 * on a failure, the kept one marked, its bundle directory.
 */
import type { CampaignResult } from "./api.ts"

export const attemptLines = (result: CampaignResult): readonly string[] => {
  const attempts = result.attempts ?? []
  return attempts.map((a, i) =>
    [
      `attempt ${i + 1}/${attempts.length}`,
      a.outcome === "failure" ? `${a.outcome} (${a.failed} failed)` : a.outcome,
      ...(a.kept ? ["kept"] : []),
      a.cell ?? "no bundle"
    ].join(" · ")
  )
}
