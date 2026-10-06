/**
 * A flows document's bytes as the segments it is made of: the envelope's head,
 * one per `legs` element, then the envelope's tail. Concatenated in order they
 * ARE the file, byte for byte. Nothing is parsed: the scanner respects strings,
 * escapes and nesting and reads no value, and it matches the `legs` key on the
 * RAW bytes, so a key written with escapes is not the one it is looking for.
 */
import { Buffer } from "node:buffer"
import * as fs from "node:fs"

/** Everything before the first leg — the `legs` key and its `[` included. */
export interface Head {
  readonly kind: "head"
  /** Offset of the segment's first byte in the file. */
  readonly at: number
  readonly bytes: Uint8Array
}

/** One element of `legs`, and the separator carrying on to the next. */
export interface LegSegment {
  readonly kind: "leg"
  /** Position in `legs`, which is the leg id every reference in the document uses. */
  readonly index: number
  /** Offset of the leg's first byte in the file. */
  readonly at: number
  /** This leg's first byte up to the next leg's, or up to the array's `]`. */
  readonly bytes: Uint8Array
  /** How many of `bytes` are the leg's own JSON text; the rest is the separator. */
  readonly json: number
}

/** The `]` closing `legs`, and everything after it. */
export interface Tail {
  readonly kind: "tail"
  /** Offset of the segment's first byte in the file. */
  readonly at: number
  readonly bytes: Uint8Array
}

export type Segment = Head | LegSegment | Tail

/** How much of the file is read at a time; a leg is far smaller. */
const CHUNK = 1 << 20

const TAB = 0x09
const LF = 0x0a
const CR = 0x0d
const SPACE = 0x20
const QUOTE = 0x22
const COMMA = 0x2c
const OPEN_BRACKET = 0x5b
const BACKSLASH = 0x5c
const CLOSE_BRACKET = 0x5d
const OPEN_BRACE = 0x7b
const CLOSE_BRACE = 0x7d

const isSpace = (byte: number): boolean =>
  byte === SPACE || byte === LF || byte === TAB || byte === CR

/**
 * Every segment of the flows document at `file`, in order.
 *
 * Refuses a document with no top-level `legs` array: a reader that quietly took
 * the whole file for its envelope would answer a capture with no calls at all.
 */
export function* segments(file: string): Generator<Segment> {
  const fd = fs.openSync(file, "r")
  try {
    // Where the current segment starts, what of it earlier chunks hold, and
    // where the leg's own text ended — the three facts a boundary needs.
    let carry: Array<Buffer> = []
    let carried = 0
    let segStart = 0
    let segStartAt = 0
    let solidAt = -1

    let phase: "before" | "legs" | "after" = "before"
    let depth = 0
    let inString = false
    let escaped = false
    let expectKey = false
    let readingKey = false
    let key: Array<number> = []
    let named: string | null = null
    let elementOpen = false
    let legs = 0
    let legEnd = 0

    const chunk = Buffer.allocUnsafe(CHUNK)
    let base = 0

    /** The current segment, closed just before `upto` in this chunk. */
    const take = (upto: number): Uint8Array => {
      const piece = chunk.subarray(segStart, upto)
      const bytes = carried === 0
        ? Buffer.from(piece)
        : Buffer.concat([...carry, piece], carried + piece.length)
      carry = []
      carried = 0
      segStart = upto
      segStartAt = base + upto
      return bytes
    }

    /** The segment that ends where a leg begins: the head, or the leg before it. */
    const closed = (upto: number): Segment => {
      // Read before `take`, which moves the start on to the next segment.
      const at = segStartAt
      if (legs === 0) return { kind: "head", at, bytes: take(upto) }
      const json = legEnd - at
      return { kind: "leg", index: legs - 1, at, bytes: take(upto), json }
    }

    for (;;) {
      const read = fs.readSync(fd, chunk, 0, CHUNK, null)
      if (read === 0) break
      for (let i = 0; i < read; i++) {
        const byte = chunk[i]!
        const at = base + i
        if (inString) {
          if (escaped) escaped = false
          else if (byte === BACKSLASH) escaped = true
          else if (byte === QUOTE) {
            inString = false
            if (readingKey) {
              named = Buffer.from(key).toString("latin1")
              readingKey = false
            }
          } else if (readingKey) key.push(byte)
          solidAt = at
          continue
        }
        if (isSpace(byte)) continue
        const previous = solidAt
        solidAt = at
        if (phase === "after") {
          if (byte === QUOTE) inString = true
          continue
        }
        if (phase === "legs" && !elementOpen) {
          if (byte === COMMA) continue
          if (byte === CLOSE_BRACKET) {
            yield closed(i)
            phase = "after"
            depth = 1
            continue
          }
          // A leg starts here, so the segment before it is whole.
          yield closed(i)
          legs++
          elementOpen = true
          if (byte === QUOTE) {
            inString = true
            continue
          }
          if (byte === OPEN_BRACE || byte === OPEN_BRACKET) depth++
          continue
        }
        if (phase === "legs") {
          if (byte === QUOTE) {
            inString = true
            continue
          }
          if (byte === CLOSE_BRACKET && depth === 2) {
            // A bare-value leg the array closes right after: it ends, then so does `legs`.
            elementOpen = false
            legEnd = previous + 1
            yield closed(i)
            phase = "after"
            depth = 1
            continue
          }
          if (byte === OPEN_BRACE || byte === OPEN_BRACKET) depth++
          else if (byte === CLOSE_BRACE || byte === CLOSE_BRACKET) {
            depth--
            if (depth === 2) {
              elementOpen = false
              legEnd = at + 1
            }
          } else if (byte === COMMA && depth === 2) {
            // A bare-value leg: its text ends where the separator starts.
            elementOpen = false
            legEnd = previous + 1
          }
          continue
        }
        if (byte === QUOTE) {
          inString = true
          if (depth === 1 && expectKey) {
            readingKey = true
            expectKey = false
            key = []
          }
          continue
        }
        if (byte === OPEN_BRACE) {
          depth++
          if (depth === 1) expectKey = true
        } else if (byte === OPEN_BRACKET) {
          if (depth === 1 && named === "legs") {
            depth = 2
            phase = "legs"
            elementOpen = false
          } else depth++
        } else if (byte === CLOSE_BRACE || byte === CLOSE_BRACKET) depth--
        else if (byte === COMMA && depth === 1) {
          expectKey = true
          named = null
        }
      }
      // Whatever of this chunk the current segment still holds travels with it.
      if (segStart < read) {
        const rest = chunk.subarray(segStart, read)
        carry.push(Buffer.from(rest))
        carried += rest.length
      }
      base += read
      segStart = 0
    }
    if (phase !== "after") {
      throw new Error(`${file}: no top-level "legs" array — not a flows document`)
    }
    yield { kind: "tail", at: segStartAt, bytes: take(0) }
  } finally {
    fs.closeSync(fd)
  }
}
