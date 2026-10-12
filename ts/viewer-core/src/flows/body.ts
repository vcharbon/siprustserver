/**
 * A message body drawn part by part: what is text is shown as text, what is
 * not is shown as a hex dump, and a multipart body keeps its boundary lines
 * and part headers between them.
 *
 * The parts are the writer's: `body.parts[]` locates each part's content in
 * the body bytes, and what lies between two parts is the multipart framing
 * (a boundary line, the next part's headers), text by construction. A body
 * with no parts is one part. A capture's message and a run's recorded line
 * draw through the same function: both carry a wire and a layout.
 */
import { bytesOfLatin1, type WireBody, type WirePayload } from "../wire/payload.ts"

/** What a body is drawn from: the wire, where its head ends, and the layout the line states. */
export interface DrawableBody {
  readonly raw: string | undefined
  readonly binary: boolean
  /** Characters of `raw` that are the head, as the arm reader measured them. */
  readonly headLength: number
  readonly body: WireBody | undefined
}

export interface BodySegment {
  /**
   * `framing`: multipart boundary lines and part headers; `text`: a part that
   * reads as text; `hex`: the dump of one that does not; `excess`: a one-line
   * label for the bytes the datagram carried past the layout's declared
   * length (RFC 3261 §18.3), never drawn as body.
   */
  readonly kind: "framing" | "text" | "hex" | "excess"
  readonly text: string
}

const utf8 = new TextDecoder("utf-8", { fatal: true })
const utf8Lenient = new TextDecoder("utf-8")
const utf8Encoder = new TextEncoder()

/** Text is UTF-8 whose only control characters are tab, CR and LF. */
const textOf = (bytes: Uint8Array): string | undefined => {
  for (const byte of bytes) {
    if (byte === 0x7f || (byte < 0x20 && byte !== 0x09 && byte !== 0x0a && byte !== 0x0d)) return undefined
  }
  try {
    return utf8.decode(bytes)
  } catch {
    return undefined
  }
}

const BYTES_PER_LINE = 16

const hex = (byte: number): string => byte.toString(16).padStart(2, "0")

const printable = (byte: number): string => (byte >= 0x20 && byte < 0x7f ? String.fromCharCode(byte) : ".")

/** `hexdump -C` shape: offset, sixteen bytes in two groups of eight, the printable-ASCII column. */
export const hexDump = (bytes: Uint8Array): string => {
  const lines: string[] = []
  for (let at = 0; at < bytes.length; at += BYTES_PER_LINE) {
    const line = bytes.subarray(at, at + BYTES_PER_LINE)
    const cells = Array.from(line, hex)
    const left = cells.slice(0, 8).join(" ")
    const right = cells.slice(8).join(" ")
    const dump = `${left}${right.length > 0 ? `  ${right}` : ""}`.padEnd(BYTES_PER_LINE * 3)
    lines.push(`${at.toString(16).padStart(4, "0")}  ${dump}  |${Array.from(line, printable).join("")}|`)
  }
  return lines.join("\n")
}

const partSegment = (bytes: Uint8Array): BodySegment => {
  const text = textOf(bytes)
  return text === undefined ? { kind: "hex", text: hexDump(bytes) } : { kind: "text", text }
}

/**
 * The tail's bytes — everything past the head the arm reader measured: one
 * per character where the wire holds bytes, the UTF-8 encoding where it is
 * text, so a part's byte offsets index them either way.
 */
export const bodyBytes = (wire: Pick<WirePayload, "raw" | "binary" | "headLength">): Uint8Array => {
  const tail = wire.raw.slice(wire.headLength)
  return wire.binary ? bytesOfLatin1(tail) : utf8Encoder.encode(tail)
}

/**
 * The body of a message or a recorded line, as the segments a viewer draws in
 * order: framing text between the parts, each part as text or hex. The body
 * is bounded by the layout's `len` where the line states one — the bound the
 * check and the confrontation read — and the bytes past it are one `excess`
 * segment, labelled, never drawn as body. Where the line states NO layout the
 * whole tail is drawn: a viewer renders what the wire carried, and is not to
 * be aligned to the confrontation, which reads such a line as bodiless. A
 * wire the line does not carry draws nothing.
 */
export const bodySegments = (message: DrawableBody): readonly BodySegment[] => {
  if (message.raw === undefined) return []
  const tail = bodyBytes({ raw: message.raw, binary: message.binary, headLength: message.headLength })
  const bound = message.body === undefined ? tail.length : Math.min(message.body.len, tail.length)
  const body = tail.subarray(0, bound)
  const excess: BodySegment[] = tail.length > bound
    ? [{ kind: "excess", text: `${tail.length - bound} byte${tail.length - bound === 1 ? "" : "s"} past the declared length` }]
    : []
  if (body.length === 0) return excess
  const parts = [...(message.body?.parts ?? [])]
    .filter((part) => part.offset >= 0 && part.offset + part.len <= body.length)
    .sort((left, right) => left.offset - right.offset)
  if (parts.length === 0) return [partSegment(body), ...excess]
  const segments: BodySegment[] = []
  let at = 0
  for (const part of parts) {
    if (part.offset > at) segments.push({ kind: "framing", text: utf8Lenient.decode(body.subarray(at, part.offset)) })
    segments.push(partSegment(body.subarray(part.offset, part.offset + part.len)))
    at = part.offset + part.len
  }
  if (at < body.length) segments.push({ kind: "framing", text: utf8Lenient.decode(body.subarray(at)) })
  return [...segments, ...excess]
}

/** The segments as one text, the way a copy or a text export carries them. */
export const bodyText = (segments: readonly BodySegment[]): string =>
  segments
    .map((segment) =>
      segment.kind === "hex" ? `${segment.text}\n` : segment.kind === "excess" ? `[${segment.text}]\n` : segment.text
    )
    .join("")
