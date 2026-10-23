/**
 * A deliberately small SIP start-line and header reader for recorded traffic.
 *
 * It exists to label an arrow and to name a transaction, never to validate:
 * a recording holds what crossed the wire, malformations included, and a
 * viewer that refused to draw them would hide exactly the interesting run.
 */
export interface ParsedSip {
  readonly kind: "request" | "response" | "unparsed"
  readonly method: string | undefined
  readonly ruri: string | undefined
  readonly status: number | undefined
  readonly reason: string | undefined
  readonly cseq: number | undefined
  readonly cseqMethod: string | undefined
  readonly callId: string | undefined
  /** The From-tag the wire carried; absent where the header stated none. */
  readonly fromTag: string | undefined
  /** The To-tag the wire carried; absent until the answerer has stated one. */
  readonly toTag: string | undefined
  readonly headers: readonly { readonly name: string; readonly value: string }[]
  readonly body: string | undefined
}

const emptyParsed: ParsedSip = {
  kind: "unparsed",
  method: undefined,
  ruri: undefined,
  status: undefined,
  reason: undefined,
  cseq: undefined,
  cseqMethod: undefined,
  callId: undefined,
  fromTag: undefined,
  toTag: undefined,
  headers: [],
  body: undefined
}

const splitHead = (raw: string): { head: string; body: string | undefined } => {
  const crlf = raw.indexOf("\r\n\r\n")
  if (crlf >= 0) return { head: raw.slice(0, crlf), body: raw.slice(crlf + 4) || undefined }
  const lf = raw.indexOf("\n\n")
  if (lf >= 0) return { head: raw.slice(0, lf), body: raw.slice(lf + 2) || undefined }
  return { head: raw, body: undefined }
}

const headerValue = (
  headers: readonly { name: string; value: string }[],
  name: string
): string | undefined => headers.find((header) => header.name.toLowerCase() === name)?.value

/** The `tag` parameter of a From/To value (RFC 3261 §20.39), absent where none is stated. */
const tagParam = (value: string | undefined): string | undefined => {
  if (value === undefined) return undefined
  const match = /(?:^|[;\s])tag=([^;,\s>]+)/i.exec(value)
  return match?.[1]
}

export const parseSip = (raw: string): ParsedSip => {
  const trimmed = raw.replace(/^\s+/, "")
  if (trimmed.length === 0) return emptyParsed
  const { head, body } = splitHead(trimmed)
  const lines = head.split(/\r?\n/)
  const startLine = lines[0] ?? ""

  const headers: { name: string; value: string }[] = []
  for (const line of lines.slice(1)) {
    if (/^[ \t]/.test(line) && headers.length > 0) {
      const last = headers[headers.length - 1]!
      headers[headers.length - 1] = { name: last.name, value: `${last.value} ${line.trim()}` }
      continue
    }
    const colon = line.indexOf(":")
    if (colon <= 0) continue
    headers.push({ name: line.slice(0, colon).trim(), value: line.slice(colon + 1).trim() })
  }

  const cseqRaw = headerValue(headers, "cseq")
  const cseqMatch = cseqRaw ? /^\s*(\d+)\s+(\S+)/.exec(cseqRaw) : null
  const common = {
    cseq: cseqMatch ? Number(cseqMatch[1]) : undefined,
    cseqMethod: cseqMatch ? cseqMatch[2] : undefined,
    callId: headerValue(headers, "call-id") ?? headerValue(headers, "i"),
    fromTag: tagParam(headerValue(headers, "from") ?? headerValue(headers, "f")),
    toTag: tagParam(headerValue(headers, "to") ?? headerValue(headers, "t")),
    headers,
    body
  }

  const response = /^SIP\/2\.0\s+(\d{3})\s*(.*)$/.exec(startLine)
  if (response) {
    return {
      ...common,
      kind: "response",
      method: undefined,
      ruri: undefined,
      status: Number(response[1]),
      reason: response[2] === "" ? undefined : response[2]
    }
  }

  const request = /^([A-Za-z]+)\s+(\S+)\s+SIP\/2\.0$/.exec(startLine)
  if (request) {
    return {
      ...common,
      kind: "request",
      method: request[1]!.toUpperCase(),
      ruri: request[2],
      status: undefined,
      reason: undefined
    }
  }

  return { ...emptyParsed, ...common, kind: "unparsed" }
}
