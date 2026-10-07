/**
 * Target refreshes a scripted party makes: where the capture shows a message
 * that refreshes a dialog's remote target name another target than the
 * dialog's last, the send carrying it states `target-refresh` (RFC 3261
 * §12.1.2, §12.2.1.2).
 *
 * The Contact is tier 1 — the replaying stack writes its own host and port —
 * so without this two messages a capture tells apart by their Contact alone
 * reach the system under test byte-identical, and it takes the second for a
 * retransmission of the first (§17.1.1). The stack writes a user part of its
 * own per refresh; the count is all the document states, never the captured
 * URI, whose user part is a number the plan owns.
 */
import { Flows } from "@sip/contracts"
import type { StepDraft } from "./draft.js"
import type { StepSource } from "./flowsteps.js"
import { headerValue } from "./wire.js"

/**
 * The remote target a Contact value names (RFC 3261 §12.2.1.2): scheme, user
 * and host:port, its parameters dropped — a parameter change alone moves no
 * target.
 */
const targetOf = (value: string): string => {
  const open = value.indexOf("<")
  const uri = open >= 0 ? value.slice(open + 1, value.indexOf(">", open) < 0 ? undefined : value.indexOf(">", open)) : value
  return (uri.split(/[;?]/)[0] ?? "").trim().toLowerCase()
}

/**
 * Whether a message may refresh its dialog's remote target (RFC 3261 §12.2,
 * RFC 3311 §5.2): an INVITE or UPDATE, or a provisional above 100 or a 2xx
 * answering one. An ACK, a BYE, its answer and every other message carry a
 * Contact that targets nothing.
 */
const refreshesTarget = (msg: Flows.Msg): boolean => {
  if (msg.summary.kind === "request") return Flows.isMethod(msg, "INVITE") || Flows.isMethod(msg, "UPDATE")
  const status = msg.summary.status
  return (Flows.isResponseTo(msg, "INVITE") || Flows.isResponseTo(msg, "UPDATE")) && status > 100 && status < 300
}

/**
 * Stamp `target-refresh` on every send that may refresh a target and names
 * another one than its dialog last did, counting per leg. The dialog side is
 * the sending party's own tag: its From-tag on a request, its To-tag on a
 * response. Mutates `steps`; `sources` is parallel to it. A step transcribed
 * from another leg's message speaks for no party of this one and is skipped.
 */
export const stampTargetRefreshes = (
  flows: Flows.FlowsDoc,
  steps: ReadonlyArray<StepDraft>,
  sources: ReadonlyArray<StepSource>
): void => {
  const targets = new Map<string, string>()
  const refreshes = new Map<string, number>()
  steps.forEach((step, i) => {
    const source = sources[i]
    if (step.op !== "send" || source === undefined || source.mirrored === true) return
    const msg = flows.legs[source.origLeg]?.msgs[source.msgIdx]
    if (msg === undefined || !refreshesTarget(msg)) return
    const value = headerValue(msg, "Contact")
    if (value === undefined) return
    const own = msg.summary.kind === "request" ? msg.summary.from.tag : msg.summary.to.tag
    const dialog = `${step.leg}\u0000${own ?? ""}`
    const target = targetOf(value)
    const seen = targets.get(dialog)
    targets.set(dialog, target)
    if (seen === undefined || seen === target) return
    const n = (refreshes.get(step.leg) ?? 0) + 1
    refreshes.set(step.leg, n)
    step.msg = { ...step.msg, "target-refresh": n }
  })
}
