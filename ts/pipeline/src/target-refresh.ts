/**
 * Target refreshes a scripted party makes: where the capture shows a party's
 * Contact change on its leg, the send that carries the new one states
 * `target-refresh` (RFC 3261 §12.1.2, §12.2.1.2).
 *
 * The Contact is tier 1 — the replaying stack writes its own host and port —
 * so without this two messages a capture tells apart by their Contact alone
 * reach the system under test byte-identical, and it takes the second for a
 * retransmission of the first (§17.1.1). The stack writes a user part of its
 * own per refresh; the count is all the document states, never the captured
 * URI, whose user part is a number the plan owns.
 */
import type { Flows } from "@sip/contracts"
import type { StepDraft } from "./draft.js"
import type { StepSource } from "./flowsteps.js"
import { headerValue } from "./wire.js"

/** The URI a Contact value names: inside its angle brackets, else up to its parameters. */
const contactUri = (value: string): string => {
  const open = value.indexOf("<")
  if (open >= 0) {
    const close = value.indexOf(">", open)
    return value.slice(open + 1, close < 0 ? undefined : close).trim()
  }
  return (value.split(";")[0] ?? "").trim()
}

/**
 * Stamp `target-refresh` on every send whose captured Contact differs from the
 * one its leg's party last sent, counting per leg. Mutates `steps`; `sources`
 * is parallel to it. A step transcribed from another leg's message speaks for
 * no party of this one and is skipped.
 */
export const stampTargetRefreshes = (
  flows: Flows.FlowsDoc,
  steps: ReadonlyArray<StepDraft>,
  sources: ReadonlyArray<StepSource>
): void => {
  const last = new Map<string, { uri: string; refreshes: number }>()
  steps.forEach((step, i) => {
    const source = sources[i]
    if (step.op !== "send" || source === undefined || source.mirrored === true) return
    const msg = flows.legs[source.origLeg]?.msgs[source.msgIdx]
    const value = msg === undefined ? undefined : headerValue(msg, "Contact")
    if (value === undefined) return
    const uri = contactUri(value)
    const seen = last.get(step.leg)
    if (seen === undefined) {
      last.set(step.leg, { uri, refreshes: 0 })
      return
    }
    if (seen.uri === uri) return
    const refreshes = seen.refreshes + 1
    last.set(step.leg, { uri, refreshes })
    step.msg = { ...step.msg, "target-refresh": refreshes }
  })
}
