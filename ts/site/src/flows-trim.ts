/**
 * A window of capture legs made smaller on request: each message's `raw`
 * kept whole (`full`), cut to its start line and headers (`head`), or left
 * out (`none`), the rest of every message as it is; or each message cut to
 * one line (`line`): its time, its addresses, its start line. What a reader
 * asks for when it wants a call's shape or headers and not every body.
 */
import type { TriageCatalog } from "./triage.ts"

export const RAW_FORMS = ["full", "head", "none", "line"] as const
export type RawForm = (typeof RAW_FORMS)[number]

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : undefined

const headOf = (raw: string): string => {
  const end = raw.indexOf("\r\n\r\n")
  return end < 0 ? raw : raw.slice(0, end + 2)
}

/** `window` with every message's `raw` in the `form` asked. */
export const trimLegs = (window: TriageCatalog.StudyFlowsLegs, form: RawForm): TriageCatalog.StudyFlowsLegs => {
  if (form === "full") return window
  return {
    from: window.from,
    legs: window.legs.map((leg) => {
      const one = record(leg)
      const msgs = one?.["msgs"]
      if (one === undefined || !Array.isArray(msgs)) return leg
      return {
        ...one,
        msgs: msgs.map((msg) => {
          const m = record(msg)
          if (m === undefined || typeof m["raw"] !== "string") return msg
          if (form === "head") return { ...m, raw: headOf(m["raw"]) }
          if (form === "line") {
            const { ts_us, src, dst, retx } = m
            return { ts_us, src, dst, retx, line: m["raw"].split("\r\n")[0] ?? "" }
          }
          const { raw: _, ...rest } = m
          return rest
        })
      }
    })
  }
}
