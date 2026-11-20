/**
 * A log read for one call, over that call's own window: first the lines
 * naming one of the call's identities (its Call-IDs, their local parts, the
 * routing exchanges' `refId`s), errors and warnings first; then the window's
 * errors and warnings that name no call at all, repeats folded and bounded —
 * labelled as the window's, never as the call's. A line naming another call
 * (a Call-ID `x@host`, a long hex `refId`) is that call's, and left out.
 */

/** The most folded window lines a cut keeps. */
export const WINDOW_LINES = 50

export interface CallLog {
  /** The lines naming the call, errors and warnings first. */
  readonly own: ReadonlyArray<string>
  /** The window's errors and warnings naming no call: one line per kind, with how often it came. */
  readonly window: ReadonlyArray<{ readonly line: string; readonly count: number }>
  /** Kinds of window line past the bound. */
  readonly windowDropped: number
  /** Lines of the log, blank ones left out. */
  readonly total: number
}

const SEVERE = /\b(ERROR|ERR|CRIT|CRITICAL|ALERT|EMERG|FATAL|WARN|WARNING)\b/

/** A token that names a call: a SIP Call-ID (`x@host`) or a long hex reference. */
const CALL_TOKEN = /[\w.+-]+@[\w.-]+|\b[0-9a-f]{32,}\b/gi

/** The identities a call's log lines may name: each Call-ID, its local part, each refId. */
export const identitiesOf = (callIds: ReadonlyArray<string>, refIds: ReadonlyArray<string>): ReadonlyArray<string> =>
  [...new Set([...callIds, ...callIds.map((id) => id.split("@")[0] ?? id), ...refIds])].filter((id) => id.length >= 6)

/** A window line's kind: the line with its stamps, numbers and hex runs folded. */
const kindOf = (line: string): string =>
  line
    .replace(/\d{4}-\d\d-\d\dT[\d:.]+Z?/g, "")
    .replace(/\b[0-9a-f]{8,}\b/gi, "#")
    .replace(/\d+/g, "#")
    .replace(/\s+/g, " ")
    .trim()

/** `text` read for the call `identities` name. */
export const callLog = (text: string, identities: ReadonlyArray<string>): CallLog => {
  const all = text.split("\n").filter((line) => line.trim() !== "")
  const names = (line: string) => identities.some((id) => line.includes(id))
  const own = all.filter(names)
  const kinds = new Map<string, { line: string; count: number }>()
  for (const line of all) {
    if (names(line) || !SEVERE.test(line) || (line.match(CALL_TOKEN) ?? []).length > 0) continue
    const kind = kindOf(line)
    const seen = kinds.get(kind)
    if (seen === undefined) kinds.set(kind, { line, count: 1 })
    else seen.count += 1
  }
  const window = [...kinds.values()]
  return {
    own: [...own.filter((l) => SEVERE.test(l)), ...own.filter((l) => !SEVERE.test(l))],
    window: window.slice(0, WINDOW_LINES),
    windowDropped: Math.max(0, window.length - WINDOW_LINES),
    total: all.length
  }
}

/** The identities as a reader reads them: a Call-ID's local part goes without saying beside the Call-ID. */
const named = (identities: ReadonlyArray<string>): ReadonlyArray<string> =>
  identities.filter((id) => !identities.some((other) => other !== id && other.startsWith(`${id}@`)))

/** A call's log as text: its own lines under their headline, then the window's under theirs. */
export const callLogText = (log: CallLog, identities: ReadonlyArray<string>): ReadonlyArray<string> => [
  log.own.length === 0
    ? `no line names this call (${named(identities).join(", ")}) among ${log.total}`
    : `${log.own.length} of ${log.total} lines name this call (${named(identities).join(", ")}); errors and warnings first`,
  ...log.own,
  ...(log.window.length === 0 ? [] : [
    `-- the window's errors and warnings naming no call: ${log.window.length} kinds, repeats folded` +
    (log.windowDropped === 0 ? "" : `, ${log.windowDropped} more kinds left out`),
    ...log.window.map((one) => `${one.count === 1 ? "" : `(×${one.count}) `}${one.line}`)
  ])
]

/** The routing exchanges' `refId`s, request and answer alike. */
export const refIdsOf = (exchanges: ReadonlyArray<{ readonly requestBody?: unknown; readonly responseBody?: unknown }>) =>
  [
    ...new Set(exchanges.flatMap((exchange) =>
      [exchange.requestBody, exchange.responseBody].flatMap((body) => {
        const refId = typeof body === "object" && body !== null ? (body as Record<string, unknown>)["refId"] : undefined
        return typeof refId === "string" ? [refId] : []
      })
    ))
  ]
