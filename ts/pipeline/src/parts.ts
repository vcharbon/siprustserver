/**
 * Decomposed multipart bodies, indexed by the captured message they belong to.
 *
 * Extraction owns MIME: `sipflow` emits each part's media type, its
 * `Content-ID`, its other entity headers and the offset/length of its content
 * inside the body, so this module LOCATES parts in bytes the document already
 * carries and never splits on a boundary. What it must never grow is a boundary
 * splitter.
 */
import type { Flows } from "@sip/contracts"
import { Wire } from "@sip/contracts"

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
  /** The part's payload, byte for byte. */
  readonly bytes: Uint8Array
}

export interface Decomposed {
  /** The body's own media type, parameters excluded. */
  readonly contentType: string
  readonly parts: ReadonlyArray<Part>
}

/** Decomposed bodies by captured message. Empty when the capture has none. */
export type PartsIndex = ReadonlyMap<Msg, Decomposed>

/**
 * The body bytes a document's message or a recording's line carries, so a
 * part's emitted byte offsets index them directly whatever arm the datagram
 * was written in.
 */
const bodyBytes = (m: Wire.Msg): Uint8Array => Wire.headBodyOf(m)?.body ?? new Uint8Array(0)

/** What a layout claims that its bytes cannot honour, each side in words. */
export interface LayoutFault {
  /** What the layout states. */
  readonly stated: string
  /** What the bytes carry. */
  readonly carried: string
}

/**
 * Whether `layout` describes `body`: its `len` is within the bytes after the
 * head, and every part lies within `len`. The tail may be LONGER than `len` —
 * a datagram carries what it carries, and the declared length bounds the body
 * (RFC 3261 §18.3) — never shorter.
 */
export const layoutFault = (tail: Uint8Array, layout: Flows.MsgBody): LayoutFault | undefined => {
  if (layout.len > tail.length) {
    return { stated: `the layout states a body of ${layout.len} bytes`, carried: `the datagram carries ${tail.length} bytes after its head` }
  }
  const past = (layout.parts ?? []).find((p) => p.offset + p.len > layout.len)
  if (past !== undefined) {
    return {
      stated: `a ${bareType(past.content_type)} part at ${past.offset}+${past.len} reaches past the body`,
      carried: `the layout states a body of ${layout.len} bytes`
    }
  }
  return undefined
}

/** `type/subtype`, parameters dropped. */
const bareType = (contentType: string): string => contentType.split(";")[0]?.trim() ?? contentType

/**
 * The parts a body's layout locates inside its bytes: one entry per part in
 * body order, each sliced by the offset and length the layout states. The
 * layout is the extractor's (`Flows.MsgBody`, and the same shape on a recorded
 * line), so this never splits on a boundary. A layout the bytes cannot honour
 * is the writer's error, thrown here; a confronter checks `layoutFault` first
 * and states it as a probe instead.
 */
export const locate = (m: Wire.Msg, layout: Flows.MsgBody): Decomposed => {
  const bytes = bodyBytes(m)
  const fault = layoutFault(bytes, layout)
  if (fault !== undefined) throw new Error(`${fault.stated}; ${fault.carried}`)
  return {
    contentType: layout.content_type,
    parts: (layout.parts ?? []).map((p) => ({
      contentType: p.content_type,
      ...(p.content_id === undefined ? {} : { contentId: p.content_id }),
      ...(p.headers === undefined || p.headers.length === 0 ? {} : { headers: p.headers }),
      bytes: bytes.slice(p.offset, p.offset + p.len)
    }))
  }
}

/** Every multipart body the document carries, keyed by its message. */
export const index = (flows: Flows.FlowsDoc): PartsIndex => {
  const out = new Map<Msg, Decomposed>()
  for (const leg of flows.legs) {
    for (const m of leg.msgs) {
      const parts = m.body?.parts
      if (m.body === undefined || parts === undefined || parts.length === 0) continue
      try {
        out.set(m, locate(m, m.body))
      } catch (e) {
        throw new Error(`${(e as Error).message} on ${leg.call_id}`)
      }
    }
  }
  return out
}
