/**
 * A replay ladder as text with nothing folded: every SIP message's own bytes
 * under its ladder line, and every HTTP exchange of the same run inline at the
 * position its stamp puts it.
 *
 * Both sides are stamped from the run's own origin — the recording in
 * microseconds, an exchange in milliseconds at the instant its request went
 * out — so one ordering holds the call and its routing consults together.
 */
import { bodySegments, bodyText } from "../flows/body.ts"
import { isMessageEvent, type LadderLayout } from "../layout/model.ts"
import { httpBodyText, httpExchangeLabel, type HttpExchange, type HttpHeader } from "../http/exchange.ts"
import { carriesWire, readBody, readPayload } from "../wire/payload.ts"
import { ladderLines, observedUs } from "./ladder-text.ts"
import { microsecondOffset } from "./offset.ts"

/** Every line of a block indented under the ladder line it belongs to. */
const indented = (text: string, by = "    "): readonly string[] =>
  text.replace(/\s+$/, "").split(/\r?\n/).map((line) => line === "" ? "" : `${by}${line}`)

const headerLines = (side: string, headers: readonly HttpHeader[]): readonly string[] =>
  headers.map((header) => `    ${side} ${header.name}: ${header.value}`)

const payload = (side: string, body: unknown): readonly string[] => {
  const text = httpBodyText(body)
  return text === "" ? [] : [`    ${side} body:`, ...indented(text, "      ")]
}

/** One exchange, whole: what was asked, with what headers, and what came back. */
export const httpExchangeToText = (exchange: HttpExchange, time: string): readonly string[] => [
  `  http#${exchange.ordinal}${time} ${httpExchangeLabel(exchange)}`,
  ...headerLines("request", exchange.requestHeaders),
  ...payload("request", exchange.requestBody),
  ...headerLines("answer", exchange.responseHeaders),
  ...payload("answer", exchange.responseBody)
]

/**
 * The recorded datagram a ladder event carries as the text the export prints,
 * from whichever arm its source line wrote it in: a text line as it is, a
 * line whose body is bytes as its head followed by the body's segments —
 * text parts as text, a binary part as a hex dump.
 */
const rawOf = (source: unknown): string | undefined => {
  if (typeof source !== "object" || source === null) return undefined
  const line = source as Record<string, unknown>
  if (!carriesWire(line)) {
    const raw = line["raw"]
    return typeof raw === "string" && raw.length > 0 ? raw : undefined
  }
  const wire = readPayload(line)
  if (wire.raw.length === 0) return undefined
  if (!wire.binary) return wire.raw
  const segments = bodySegments({ ...wire, body: readBody(line["body"]) })
  return wire.raw.slice(0, wire.headLength) + bodyText(segments)
}

/**
 * The ladder, its messages' bytes and its run's HTTP exchanges as one text.
 *
 * An exchange precedes every message its stamp is not later than, so on a tie
 * the consult reads first — it is what the message that follows was decided
 * by. An exchange is stamped in milliseconds against a ladder in microseconds,
 * so the tie is a whole millisecond wide; exchanges the ladder has no time to
 * compare against follow the last line.
 */
export const fullLadderToText = (
  layout: LadderLayout,
  exchanges: readonly HttpExchange[]
): string => {
  const rendered = ladderLines(layout)
  const observed = layout.events.map(observedUs).filter((us): us is number => us !== undefined)
  const t0 = observed.length === 0 ? undefined : Math.min(...observed)
  const timeOf = (atUs: number): string => t0 === undefined ? "" : ` ${microsecondOffset(atUs - t0)}`
  const ordered = [...exchanges].sort((a, b) => a.ordinal - b.ordinal)

  const out: string[] = [...rendered.head]
  let next = 0
  const emit = (exchange: HttpExchange): void => {
    out.push(...httpExchangeToText(exchange, timeOf(exchange.atMs * 1000)))
    next += 1
  }
  /** Every exchange no later than this message, before the message itself. */
  const drainBefore = (atUs: number): void => {
    while (next < ordered.length && ordered[next]!.atMs * 1000 <= atUs) emit(ordered[next]!)
  }
  for (const row of rendered.lines) {
    const atUs = observedUs(row.event)
    if (atUs !== undefined) drainBefore(atUs)
    out.push(row.line)
    const raw = isMessageEvent(row.event) ? rawOf(row.event.source) : undefined
    if (raw !== undefined) out.push(...indented(raw))
  }
  while (next < ordered.length) emit(ordered[next]!)
  out.push("")
  return out.join("\n")
}
