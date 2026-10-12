/**
 * Run-bundle recording parser: `recording/<leg>.jsonl`, one verbatim message
 * per line with its arrival time.
 *
 * A line carries its datagram in one of the three arms a capture's message
 * does (`../wire/payload.ts`), beside the body's layout. The parser is
 * tolerant on purpose — it accepts a wire in any arm or an already decoded
 * `msg` object, and either microsecond or millisecond timestamps — and
 * reports every line it could not read instead of dropping it silently.
 */
import { carriesWire, readBody, readPayload, type WireBody } from "../wire/payload.ts"
import { parseSip } from "./sip.ts"

export interface RecordedMessage {
  /** Order within the leg, as recorded. */
  readonly seq: number
  readonly leg: string
  /** `out`: the leg's actor emitted it. `in`: it arrived at the actor. */
  readonly dir: "in" | "out"
  /** Microseconds from the start of the run. */
  readonly atUs: number
  /** The flow step the interpreter was on, when it said so. */
  readonly stepId: string | undefined
  readonly method: string | undefined
  readonly status: number | undefined
  readonly reason: string | undefined
  readonly cseq: number | undefined
  readonly cseqMethod: string | undefined
  readonly callId: string | undefined
  /** The From-tag the wire carried; absent on a pre-decoded line or a tagless header. */
  readonly fromTag: string | undefined
  /** The To-tag the wire carried; absent until the answerer has stated one. */
  readonly toTag: string | undefined
  readonly headers: readonly { readonly name: string; readonly value: string }[]
  /**
   * The wire as a string: the text arm verbatim, or the head followed by its
   * body with one character per byte (latin1); absent where the line carries
   * no wire.
   */
  readonly raw: string | undefined
  /** The datagram's length on the wire, in bytes. */
  readonly bytes: number
  /** The body (or, for an opaque datagram, the whole) is not UTF-8: `raw` holds it byte per character. */
  readonly binary: boolean
  /** Characters of `raw` that are the head, as the arm reader measured them. */
  readonly headLength: number
  /** The body's layout the line states, absent where it states none. */
  readonly body: WireBody | undefined
  /** The recorded line, verbatim, for the side panel. */
  readonly source: unknown
}

export interface RunRecording {
  readonly legs: readonly string[]
  /** Every message of every leg, ordered by arrival. */
  readonly messages: readonly RecordedMessage[]
  readonly warnings: readonly string[]
}

const asNumber = (value: unknown): number | undefined =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined

const asString = (value: unknown): string | undefined =>
  typeof value === "string" && value.length > 0 ? value : undefined

const timestampUs = (line: Record<string, unknown>): number | undefined => {
  const us = asNumber(line["at_us"]) ?? asNumber(line["atUs"])
  if (us !== undefined) return us
  const ms = asNumber(line["at_ms"]) ?? asNumber(line["atMs"])
  return ms === undefined ? undefined : ms * 1000
}

const direction = (value: unknown): "in" | "out" | undefined => {
  if (value === "in" || value === "rx" || value === "recv") return "in"
  if (value === "out" || value === "tx" || value === "sent" || value === "send") return "out"
  return undefined
}

/**
 * Parses one leg file. `leg` is the fallback leg id: a line may name its own,
 * but the file name is what a bundle keys the leg on.
 */
export const parseRecordingLeg = (
  leg: string,
  text: string
): { readonly messages: readonly RecordedMessage[]; readonly warnings: readonly string[] } => {
  const messages: RecordedMessage[] = []
  const warnings: string[] = []
  const lines = text.split(/\r?\n/)

  lines.forEach((rawLine, index) => {
    const trimmed = rawLine.trim()
    if (trimmed.length === 0) return
    let parsed: unknown
    try {
      parsed = JSON.parse(trimmed)
    } catch {
      warnings.push(`${leg}.jsonl:${index + 1}: not JSON`)
      return
    }
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      warnings.push(`${leg}.jsonl:${index + 1}: not a JSON object`)
      return
    }
    const line = parsed as Record<string, unknown>
    const dir = direction(line["dir"] ?? line["direction"])
    if (dir === undefined) {
      warnings.push(`${leg}.jsonl:${index + 1}: no readable direction`)
      return
    }
    const atUs = timestampUs(line)
    if (atUs === undefined) {
      warnings.push(`${leg}.jsonl:${index + 1}: no readable timestamp`)
    }

    const wire = carriesWire(line)
      ? readPayload(line)
      : { raw: asString(line["message"]) ?? "", bytes: 0, binary: false, headLength: 0 }
    const raw = wire.raw.length > 0 ? wire.raw : undefined
    const decoded = raw === undefined ? undefined : parseSip(raw)
    const msg = typeof line["msg"] === "object" && line["msg"] !== null
      ? (line["msg"] as Record<string, unknown>)
      : undefined

    const method = asString(msg?.["method"]) ?? decoded?.method
    const status = asNumber(msg?.["status"]) ?? decoded?.status
    if (method === undefined && status === undefined) {
      warnings.push(`${leg}.jsonl:${index + 1}: neither a method nor a status`)
    }

    messages.push({
      seq: asNumber(line["seq"]) ?? messages.length + 1,
      leg: asString(line["leg"]) ?? leg,
      dir,
      atUs: atUs ?? 0,
      stepId: asString(line["step"]) ?? asString(line["step_id"]),
      method,
      status,
      reason: asString(msg?.["reason"]) ?? decoded?.reason,
      cseq: asNumber(msg?.["cseq"]) ?? decoded?.cseq,
      cseqMethod: asString(msg?.["cseq-method"]) ?? asString(msg?.["cseqMethod"]) ?? decoded?.cseqMethod,
      callId: asString(msg?.["call-id"]) ?? decoded?.callId,
      fromTag: asString(msg?.["from-tag"]) ?? decoded?.fromTag,
      toTag: asString(msg?.["to-tag"]) ?? decoded?.toTag,
      headers: decoded?.headers ?? [],
      raw,
      bytes: raw === undefined ? 0 : wire.bytes,
      binary: wire.binary,
      headLength: wire.headLength,
      body: readBody(line["body"]),
      source: line
    })
  })

  return { messages, warnings }
}

/** Parses a whole bundle: one entry per `recording/<leg>.jsonl`. */
export const parseRecording = (files: Readonly<Record<string, string>>): RunRecording => {
  const legs = Object.keys(files).sort()
  const messages: RecordedMessage[] = []
  const warnings: string[] = []
  for (const leg of legs) {
    const parsed = parseRecordingLeg(leg, files[leg]!)
    messages.push(...parsed.messages)
    warnings.push(...parsed.warnings)
  }
  messages.sort((left, right) => left.atUs - right.atUs || left.seq - right.seq)
  return { legs, messages, warnings }
}
