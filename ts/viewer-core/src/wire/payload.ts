/**
 * The datagram a document line carries, read off whichever of the three arms
 * the writer chose (`raw` | `head` + `body_b64` | `raw_b64`): a capture's
 * message and a run's recorded line share the encoding, so they share this
 * reader. Beside it, the body's layout the writer states — media type, byte
 * length, the MIME parts located by offset — so a body is drawn part by part
 * without ever splitting on a boundary.
 *
 * The viewer's own decoder by design: a contract-free browser core reads a
 * document it does not validate (ADR-0035 in the upstream repository names
 * this exception).
 */

/** The wire as the viewer holds it: text, or one character per byte. */
export interface WirePayload {
  /**
   * The wire as a string: the text arm verbatim, or the head followed by its
   * body with one character per byte (latin1), so every byte survives.
   * Empty when the line carried none.
   */
  readonly raw: string
  /** The datagram's length on the wire, in bytes. */
  readonly bytes: number
  /** The body (or, for an opaque datagram, the whole) is not UTF-8: `raw` holds it byte per character. */
  readonly binary: boolean
  /**
   * How many characters of `raw` are the head, blank line included: the
   * split the arm reader knows, so no drawer re-searches it in text a binary
   * body may itself contain. `raw.length` where the head is unterminated.
   */
  readonly headLength: number
}

/** One entity header of a MIME part, as the part wrote it. */
export interface WirePartHeader {
  readonly name: string
  readonly value: string
}

/** One MIME part, located in the body bytes: `offset`/`len` index the part's content, past its headers. */
export interface WireBodyPart {
  readonly contentType: string
  readonly headers: readonly WirePartHeader[]
  readonly offset: number
  readonly len: number
}

/** The body's layout as the writer read it; a multipart body arrives already split. */
export interface WireBody {
  readonly contentType: string
  readonly len: number
  readonly parts: readonly WireBodyPart[]
}

/** The keys a line may carry its datagram under, one arm at a time. */
export const ARM_KEYS = ["raw", "head", "body_b64", "raw_b64"] as const

const record = (value: unknown): Record<string, unknown> | undefined =>
  typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined

const str = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined)
const num = (value: unknown): number | undefined => (typeof value === "number" ? value : undefined)

const utf8 = new TextEncoder()

/** Standard base64 to one character per byte, the way the writer's bytes are read back. */
const latin1OfBase64 = (encoded: string): string => atob(encoded)

/** The bytes a latin1 string carries, one per character. */
export const bytesOfLatin1 = (latin1: string): Uint8Array => Uint8Array.from(latin1, (char) => char.charCodeAt(0) & 0xff)

/**
 * Where a head rendered as text ends: just past the empty line that ends the
 * header block (RFC 3261 §7), CRLF being the line terminator (§25.1) and a
 * bare CR or LF accepted as the parser accepts it — a stated leniency, so
 * the parser and this reader end the head at the same byte; `text.length`
 * where the head is unterminated.
 */
export const headEndOf = (text: string): number => {
  let i = 0
  for (;;) {
    if (i >= text.length) return text.length
    const lineStart = i
    while (i < text.length && text[i] !== "\r" && text[i] !== "\n") i++
    const empty = i === lineStart
    if (i < text.length && text[i] === "\r") i++
    if (i < text.length && text[i] === "\n") i++
    if (empty) return i
    if (i === lineStart) return text.length
  }
}

/**
 * The wire's text, its byte length and whether it holds bytes rather than
 * text, from whichever of the line's three arms it carries. A line with no
 * arm reads as an empty wire.
 */
export const readPayload = (line: Record<string, unknown>): WirePayload => {
  const raw = str(line["raw"])
  if (raw !== undefined) return { raw, bytes: utf8.encode(raw).length, binary: false, headLength: headEndOf(raw) }
  const head = str(line["head"])
  const bodyB64 = str(line["body_b64"])
  if (head !== undefined && bodyB64 !== undefined) {
    const body = latin1OfBase64(bodyB64)
    return { raw: head + body, bytes: utf8.encode(head).length + body.length, binary: true, headLength: head.length }
  }
  const rawB64 = str(line["raw_b64"])
  if (rawB64 !== undefined) {
    const whole = latin1OfBase64(rawB64)
    return { raw: whole, bytes: whole.length, binary: true, headLength: headEndOf(whole) }
  }
  return { raw: "", bytes: 0, binary: false, headLength: 0 }
}

/** Whether the line carries its datagram in one of the three arms. */
export const carriesWire = (line: Record<string, unknown>): boolean =>
  str(line["raw"]) !== undefined ||
  (str(line["head"]) !== undefined && str(line["body_b64"]) !== undefined) ||
  str(line["raw_b64"]) !== undefined

const readPartHeaders = (value: unknown): readonly WirePartHeader[] =>
  (Array.isArray(value) ? value : []).flatMap((entry) => {
    const header = record(entry)
    const name = str(header?.["name"])
    const headerValue = str(header?.["value"])
    return name === undefined || headerValue === undefined ? [] : [{ name, value: headerValue }]
  })

/** The body layout a line states, or undefined where it states none the viewer can read. */
export const readBody = (value: unknown): WireBody | undefined => {
  const body = record(value)
  const contentType = str(body?.["content_type"])
  const len = num(body?.["len"])
  if (body === undefined || contentType === undefined || len === undefined) return undefined
  const parts = (Array.isArray(body["parts"]) ? body["parts"] : []).flatMap((entry) => {
    const part = record(entry)
    const partType = str(part?.["content_type"])
    const offset = num(part?.["offset"])
    const partLen = num(part?.["len"])
    if (part === undefined || partType === undefined || offset === undefined || partLen === undefined) return []
    return [{ contentType: partType, headers: readPartHeaders(part["headers"]), offset, len: partLen }]
  })
  return { contentType, len, parts }
}
