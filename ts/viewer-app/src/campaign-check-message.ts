/**
 * The ladder message a campaign check was made at: the first recorded message
 * the check's flow step decorates (a retransmission repeats the step, the
 * first send is the one cited). A check with no step, or a step no message
 * carries, has none.
 */
import type { LadderEvent } from "@sip/viewer-core"
import type { CampaignCheck } from "./api.ts"

/** The id of the message each check of `checks` was made at, in check order; undefined where none. */
export const checkMessages = (
  checks: ReadonlyArray<Pick<CampaignCheck, "step">>,
  events: ReadonlyArray<LadderEvent>
): ReadonlyArray<string | undefined> => {
  const byStep = new Map<string, string>()
  for (const event of events) {
    for (const decoration of event.decorations) {
      if (decoration.kind === "step" && !byStep.has(decoration.id)) byStep.set(decoration.id, event.id)
    }
  }
  return checks.map((check) => (check.step === undefined || check.step === null ? undefined : byStep.get(check.step)))
}
