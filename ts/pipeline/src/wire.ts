/**
 * Header list and body access over the datagram a flows message or a recorded
 * line carries, decoded through `Contracts.Wire`: the head is read as text,
 * the body is handed over as bytes.
 *
 * STOPGAP. Pivot emission needs EVERY header in wire order with casing and
 * duplicates preserved (the tier-3 freeze surface), which is a full header-list
 * parse in JS. It belongs in the Rust emitter, and this module is INTERNAL to
 * the package for exactly that reason: nothing outside it may grow a dependency
 * on a SIP parse living here.
 */
import { Wire } from "@sip/contracts"

/** RFC 3261 compact forms of the header names the tier model names. */
const COMPACT: Record<string, string> = {
  v: "via",
  i: "call-id",
  f: "from",
  t: "to",
  m: "contact",
  l: "content-length",
  c: "content-type",
  e: "content-encoding",
  s: "subject",
  k: "supported",
  r: "refer-to",
  b: "referred-by",
  o: "event",
  x: "session-expires"
}

/** Whether two header-name spellings name the same header. */
export const sameHeader = (a: string, b: string): boolean => canon(a) === canon(b)

/** The canonical (lowercase, compact-form-resolved) spelling of a header name. */
export const canonicalName = (name: string): string => canon(name)

const canon = (name: string): string => {
  const lower = name.trim().toLowerCase()
  return COMPACT[lower] ?? lower
}

/**
 * The datagram's head as text and its body as bytes, through the one decoder
 * (`Contracts.Wire`). An opaque datagram states no head at all: there is
 * nothing to read there, and pretending otherwise would invent headers the
 * capture never carried.
 */
const headBody = (m: Wire.Msg): readonly [string, Uint8Array] => {
  const split = Wire.headBodyOf(m)
  return split === undefined ? ["", new Uint8Array(0)] : [split.head, split.body]
}

const splitHead = (raw: string): string => {
  const i = raw.indexOf("\r\n\r\n")
  if (i >= 0) return raw.slice(0, i)
  const j = raw.indexOf("\n\n")
  if (j >= 0) return raw.slice(0, j)
  return raw
}

/** The datagram's head as text: the start line and the header block, unparsed. */
export const headText = (m: Wire.Msg): string => headBody(m)[0]

export interface WireHeader {
  readonly name: string
  readonly value: string
}

/**
 * Every header as `(name, value)` in wire order, casing and duplicates
 * preserved, continuation lines unfolded to one space.
 */
export const headersInOrder = (m: Wire.Msg): ReadonlyArray<WireHeader> => headersOfHead(headBody(m)[0])

/** {@link headersInOrder} over a head rendered as text (a body after the blank line is ignored). */
export const headersInOrderRaw = (raw: string): ReadonlyArray<WireHeader> => headersOfHead(splitHead(raw))

/** The body bytes a datagram carries: everything past the blank line, empty where none. */
export const bodyBytesOf = (m: Wire.Msg): Uint8Array => headBody(m)[1]

const headersOfHead = (head: string): ReadonlyArray<WireHeader> => {
  const lines = head.split(/\r?\n/).slice(1)
  const out: Array<WireHeader> = []
  for (const line of lines) {
    if (/^[ \t]/.test(line)) {
      const last = out[out.length - 1]
      if (last) out[out.length - 1] = { name: last.name, value: `${last.value} ${line.trim()}` }
      continue
    }
    const colon = line.indexOf(":")
    if (colon < 0) continue
    out.push({ name: line.slice(0, colon).trim(), value: line.slice(colon + 1).trim() })
  }
  return out
}

/** A datagram's first line, parsed just far enough to know what it is. */
export type StartLine =
  | { readonly kind: "request"; readonly method: string; readonly uri: string }
  | { readonly kind: "response"; readonly status: number; readonly reason: string }

/** The start line of a head rendered as text, or `undefined` when it is neither shape. */
export const startLineOf = (raw: string): StartLine | undefined => {
  const line = (raw.split(/\r?\n/)[0] ?? "").trim()
  const response = /^SIP\/2\.0\s+(\d{3})\s*(.*)$/.exec(line)
  if (response) return { kind: "response", status: Number(response[1]), reason: response[2] ?? "" }
  const request = /^([A-Za-z]+)\s+(\S+)\s+SIP\/2\.0$/.exec(line)
  if (request) return { kind: "request", method: (request[1] ?? "").toUpperCase(), uri: request[2] ?? "" }
  return undefined
}

/** Every value a header name carries in a wire-order list, duplicates kept. */
export const headerValuesOf = (
  headers: ReadonlyArray<WireHeader>,
  name: string
): ReadonlyArray<string> => headers.filter((h) => sameHeader(h.name, name)).map((h) => h.value)

export const hasHeader = (m: Wire.Msg, name: string): boolean =>
  headersInOrder(m).some((h) => sameHeader(h.name, name))

export const headerValue = (m: Wire.Msg, name: string): string | undefined =>
  headersInOrder(m).find((h) => sameHeader(h.name, name))?.value

export interface BodyPayload {
  /**
   * The `Content-Type` value as the datagram wrote it, PARAMETERS INCLUDED —
   * what the pivot stores, so emission puts the captured type back byte for
   * byte. Empty when the datagram states none.
   */
  readonly contentType: string
  /** `type/subtype`, parameters excluded: the body registry's lookup key. */
  readonly mediaType: string
  readonly boundary?: string
  /**
   * `contentType` with its `boundary` parameter removed, on a multipart body.
   * The boundary is the ONE parameter emission regenerates (§8.3); every other
   * parameter of the container type rides through.
   */
  readonly containerType?: string
  /** The payload, byte for byte. */
  readonly bytes: Uint8Array
}

/** The datagram's body plus its media type, or `undefined` when there is none. */
export const body = (m: Wire.Msg): BodyPayload | undefined => {
  const [, payload] = headBody(m)
  if (payload.length === 0) return undefined
  const contentType = (headerValue(m, "Content-Type") ?? "").trim()
  const [head = "", ...params] = mimeParams(contentType)
  const boundary = params
    .map((p) => /^\s*boundary\s*=\s*"?([^"]*)"?\s*$/i.exec(p)?.[1])
    .find((v) => v !== undefined)
  const kept = params.filter((p) => !/^\s*boundary\s*=/i.test(p))
  return {
    contentType,
    mediaType: head.trim(),
    boundary,
    ...(boundary === undefined ? {} : { containerType: [head, ...kept].join(";") }),
    bytes: payload
  }
}

/**
 * A media type split on its parameter separators — the type first, then one
 * entry per parameter, each VERBATIM. A `;` inside a quoted string separates
 * nothing (RFC 2045 §5.1), so a `boundary="a;b"` stays one parameter.
 */
const mimeParams = (value: string): ReadonlyArray<string> => {
  const out: Array<string> = []
  let start = 0
  let quoted = false
  for (let i = 0; i < value.length; i += 1) {
    const c = value[i]
    if (c === '"') quoted = !quoted
    else if (c === ";" && !quoted) {
      out.push(value.slice(start, i))
      start = i + 1
    }
  }
  out.push(value.slice(start))
  return out
}

/**
 * User-part identity of a URI: the sip/sips user or tel number with user
 * parameters stripped, a `urn:` target verbatim, empty for a user-less URI.
 */
export const uriUser = (uri: string): string => {
  const text = uri.trim()
  if (/^urn:/i.test(text)) return text
  const tel = /^tel:(.*)$/i.exec(text)
  if (tel) return (tel[1] ?? "").split(/[;>?]/)[0] ?? ""
  const sip = /^sips?:(.*)$/i.exec(text)
  if (!sip) return ""
  const rest = sip[1] ?? ""
  const at = rest.indexOf("@")
  if (at < 0) return ""
  return (rest.slice(0, at).split(";")[0] ?? "")
}

/** The `user@hostport` an address names, scheme and parameters dropped. */
export const addrForm = (uri: string): string => {
  const text = uri.trim()
  const sip = /^sips?:(.*)$/i.exec(text)
  if (!sip) return text
  const rest = sip[1] ?? ""
  const at = rest.indexOf("@")
  const user = at < 0 ? undefined : (rest.slice(0, at).split(";")[0] ?? "")
  const authority = (at < 0 ? rest : rest.slice(at + 1)).split(/[;>?]/)[0] ?? ""
  return user === undefined ? authority : `${user}@${authority}`
}
