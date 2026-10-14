/** Human-facing one-liners for the things a ladder draws. */
import type { Body, MsgSpec, Step } from "../generated/pivot-v3.ts"

/** The label a message draws on its arrow. */
export const msgLabel = (msg: MsgSpec): string => {
  if (typeof msg.status === "number") {
    return msg.reason ? `${msg.status} ${msg.reason}` : String(msg.status)
  }
  return msg.method ?? "?"
}

/** The dimmer second line: the transaction a message belongs to. */
export const msgSublabel = (msg: MsgSpec): string | undefined => {
  const parts: string[] = []
  const cseqMethod = msg["cseq-method"]
  if (typeof msg.status === "number" && cseqMethod) parts.push(cseqMethod)
  if (typeof msg.cseq === "number") parts.push(`CSeq ${msg.cseq}`)
  return parts.length === 0 ? undefined : parts.join(" · ")
}

export const stepLabel = (step: Step): string => msgLabel(step.msg)
export const stepSublabel = (step: Step): string | undefined => msgSublabel(step.msg)

/** A short description of a body, for a decoration badge. */
export const bodySummary = (body: Body): string => {
  if ("multipart" in body) {
    const parts = body.multipart.parts ?? []
    return `${body.multipart["content-type"]} (${parts.length} parts)`
  }
  if ("ref" in body) {
    const rewrite = body.rewrite ?? []
    return rewrite.length > 0 ? `${body.ref} +rewrite` : body.ref
  }
  return body.mode
}
