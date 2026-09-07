/**
 * Decomposed multipart bodies, indexed by the captured message they belong to.
 *
 * Extraction owns MIME: `sipflow` emits each part's media type, its
 * `Content-ID`, its other entity headers and the offset/length of its content
 * inside the body, so this module LOCATES parts in bytes the document already
 * carries and never splits on a boundary. What it must never grow is a boundary
 * splitter.
 */
import { Flows } from "@sip/contracts"

type Msg = Flows.Msg

/** One MIME part as extraction hands it over. */
export interface Part {
  readonly contentType: string
  /** The part's `Content-ID`, angle brackets included, when it states one. */
  readonly contentId?: string
  /**
   * The part's remaining entity headers in wire order (RFC 2045 §3) — its
   * `Content-Type` and `Content-ID` are the fields above, never repeated here.
   */
  readonly headers?: ReadonlyArray<{ readonly name: string; readonly value: string }>
  /** Payload bytes as a latin1 string, so a binary part survives verbatim. */
  readonly text: string
}

export interface Decomposed {
  /** The body's own media type, parameters excluded. */
  readonly contentType: string
  readonly parts: ReadonlyArray<Part>
}

/** Decomposed bodies by captured message. Empty when the capture has none. */
export type PartsIndex = ReadonlyMap<Msg, Decomposed>

/**
 * The message's body as latin1 bytes — one character per byte, so a part's
 * emitted byte offsets index it directly even when the datagram arrived as
 * UTF-8 text carrying multi-byte characters.
 */
const bodyBytes = (m: Msg): string => {
  const payload = Flows.payloadOf(m)
  if (payload._tag === "text") {
    const all = Buffer.from(payload.raw, "utf8")
    const at = all.indexOf("\r\n\r\n")
    return at < 0 ? "" : all.subarray(at + 4).toString("latin1")
  }
  if (payload._tag === "head-body") return Buffer.from(payload.body_b64, "base64").toString("latin1")
  return ""
}

/** Every multipart body the document carries, keyed by its message. */
export const index = (flows: Flows.FlowsDoc): PartsIndex => {
  const out = new Map<Msg, Decomposed>()
  for (const leg of flows.legs) {
    for (const m of leg.msgs) {
      const parts = m.body?.parts
      if (m.body === undefined || parts === undefined || parts.length === 0) continue
      const bytes = bodyBytes(m)
      if (bytes.length !== m.body.len) {
        throw new Error(`body length ${bytes.length} != emitted ${m.body.len} on ${leg.call_id}`)
      }
      out.set(m, {
        contentType: m.body.content_type,
        parts: parts.map((p) => ({
          contentType: p.content_type,
          ...(p.content_id === undefined ? {} : { contentId: p.content_id }),
          ...(p.headers === undefined || p.headers.length === 0 ? {} : { headers: p.headers }),
          text: bytes.slice(p.offset, p.offset + p.len)
        }))
      })
    }
  }
  return out
}
