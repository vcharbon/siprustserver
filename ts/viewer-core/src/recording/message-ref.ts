/**
 * A recorded message named the way a run's audit names it — the leg whose
 * recording holds it and that leg's `seq` — found on a replay ladder. The
 * ladder's own number (`m<row>`) is what every reader shows for it; a finding
 * that names no message, as an audit written before it did, finds none.
 */
import { isMessageEvent, type LadderLayout, type MessageEvent } from "../layout/model.ts"

/** One recorded message: its leg, and its `seq` on that leg. */
export interface MessageRef {
  readonly leg: string
  readonly seq: number
}

/** The `offending_message` a finding states, where it states a readable one. */
export const messageRefOf = (finding: unknown): MessageRef | undefined => {
  const value = typeof finding === "object" && finding !== null
    ? (finding as Record<string, unknown>)["offending_message"]
    : undefined
  if (typeof value !== "object" || value === null) return undefined
  const { leg, seq } = value as Record<string, unknown>
  return typeof leg === "string" && typeof seq === "number" ? { leg, seq } : undefined
}

/** The ladder event of `ref` on a replay `layout`; none where the ladder holds no such message. */
export const eventOfRef = (layout: LadderLayout, ref: MessageRef): MessageEvent | undefined =>
  layout.events.find((event): event is MessageEvent => {
    if (!isMessageEvent(event) || event.legId !== ref.leg) return false
    const source = event.source
    return typeof source === "object" && source !== null && (source as Record<string, unknown>)["seq"] === ref.seq
  })
