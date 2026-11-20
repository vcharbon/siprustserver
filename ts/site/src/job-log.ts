/**
 * A job's log, a byte range at a time: the page tails the
 * file by offset and the server reads the range asked for — never the whole
 * file per poll. A range past the end is empty, a range that ends inside a
 * line stops at the last newline before it so a poll never shows half a line,
 * and one that ends inside a multi-byte character backs off to the character
 * boundary so nothing is mangled across polls. Whatever the range, `to` is
 * where the next poll starts.
 */
import * as Effect from "effect/Effect"
import * as fs from "node:fs"

export interface Range {
  /** The offset read from, as asked (clamped to the file's size). */
  readonly from: number
  /** The offset the next poll starts at: `from` plus the bytes delivered. */
  readonly to: number
  readonly text: string
  /** The file's size at the read: `to < size` means more is there already. */
  readonly size: number
}

/** The most bytes one poll delivers: a log that grew by more is caught up over the next polls. */
export const CAP = 512 * 1024

/** Bytes that are a UTF-8 continuation (`10xxxxxx`). */
const isContinuation = (byte: number): boolean => (byte & 0xc0) === 0x80

/** How many bytes a lead byte announces; 1 for ASCII and for anything that is not a lead. */
const sequenceLength = (lead: number): number =>
  (lead & 0xf8) === 0xf0 ? 4 : (lead & 0xf0) === 0xe0 ? 3 : (lead & 0xe0) === 0xc0 ? 2 : 1

/** The length of the buffer's prefix that ends on a character boundary. */
export const completeLength = (buffer: Uint8Array): number => {
  let at = buffer.length - 1
  let back = 0
  while (at >= 0 && back < 3 && isContinuation(buffer[at]!)) {
    at -= 1
    back += 1
  }
  if (at < 0) return buffer.length
  const needed = sequenceLength(buffer[at]!)
  return back + 1 >= needed ? buffer.length : at
}

/** The length of the buffer's prefix that ends on its last newline; the whole buffer where it holds none. */
export const lineLength = (buffer: Uint8Array): number => {
  const last = buffer.lastIndexOf(0x0a)
  return last < 0 ? buffer.length : last + 1
}

/** Reads `[from, min(from + cap, size))` of the file, stopped at a line and a character boundary unless it reaches the end. */
export const readRange = (path: string, from: number, cap = CAP): Effect.Effect<Range, Error> =>
  Effect.try({
    try: () => {
      const fd = fs.openSync(path, "r")
      try {
        const size = fs.fstatSync(fd).size
        const start = Math.min(Math.max(0, Math.floor(from)), size)
        const want = Math.min(cap, size - start)
        if (want === 0) return { from: start, to: start, text: "", size }
        const buffer = Buffer.alloc(want)
        const read = fs.readSync(fd, buffer, 0, want, start)
        const got = buffer.subarray(0, read)
        // Short of the end, the chunk stops at its last newline; with none (a line longer than the
        // cap), or at the end, it stops at the last character boundary instead.
        const atEnd = start + read >= size
        const length = atEnd || got.lastIndexOf(0x0a) < 0 ? completeLength(got) : lineLength(got)
        return { from: start, to: start + length, text: got.subarray(0, length).toString("utf8"), size }
      } finally {
        fs.closeSync(fd)
      }
    },
    catch: (cause) => (cause instanceof Error ? cause : new Error(String(cause)))
  })

export * as JobLog from "./job-log.ts"
