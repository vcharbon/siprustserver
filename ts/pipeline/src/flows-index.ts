/**
 * Where each leg of a flows document sits in its file: the envelope's two
 * halves held, every leg named by its offset and length, no leg read. One
 * scan of the file builds it (`flows-segments.ts`, no JSON), and a reader
 * that wants a few legs then costs a few positioned reads, whatever the
 * document's size.
 *
 * Cached per process by path, and trusted only while the file's size and
 * mtime are the ones it was built from: a document rewritten under the same
 * name is re-scanned, never read off stale offsets.
 */
import { Buffer } from "node:buffer"
import * as fs from "node:fs"
import { segments } from "./flows-segments.js"

export interface FlowsIndex {
  readonly file: string
  /** The envelope's text before the first leg, `legs` key and `[` included. */
  readonly head: string
  /** The envelope's text from the `]` closing `legs`. */
  readonly tail: string
  /** Leg index → offset of the leg's first byte in the file. */
  readonly at: Float64Array
  /** Leg index → the byte length of the leg's own JSON text. */
  readonly length: Uint32Array
}

interface Cached {
  readonly size: number
  readonly mtimeMs: number
  readonly index: FlowsIndex
}

const cache = new Map<string, Cached>()

const build = (file: string): FlowsIndex => {
  const at: Array<number> = []
  const length: Array<number> = []
  let head = ""
  let tail = ""
  for (const segment of segments(file)) {
    if (segment.kind === "leg") {
      at.push(segment.at)
      length.push(segment.json)
    } else if (segment.kind === "head") head = Buffer.from(segment.bytes).toString("utf8")
    else tail = Buffer.from(segment.bytes).toString("utf8")
  }
  return { file, head, tail, at: Float64Array.from(at), length: Uint32Array.from(length) }
}

/** The index of the document at `file`, built on first sight and while the file stands. */
export const indexOf = (file: string): FlowsIndex => {
  const stat = fs.statSync(file)
  const known = cache.get(file)
  if (known !== undefined && known.size === stat.size && known.mtimeMs === stat.mtimeMs) {
    return known.index
  }
  const index = build(file)
  cache.set(file, { size: stat.size, mtimeMs: stat.mtimeMs, index })
  return index
}

/** How many legs the document holds. */
export const legCount = (index: FlowsIndex): number => index.at.length

/**
 * The JSON text of the named legs, each read in place, keyed by its index. An
 * index the document has no leg for is left out, as `legs[i]` on the whole
 * document is `undefined`: a coordinate past the document cites nothing.
 */
export const legTexts = (index: FlowsIndex, legs: Iterable<number>): ReadonlyMap<number, string> => {
  const out = new Map<number, string>()
  const wanted = [...new Set(legs)].sort((a, b) => a - b)
  if (wanted.length === 0) return out
  const fd = fs.openSync(index.file, "r")
  try {
    for (const leg of wanted) {
      const at = index.at[leg]
      const length = index.length[leg]
      if (at === undefined || length === undefined) continue
      const buffer = Buffer.allocUnsafe(length)
      let read = 0
      while (read < length) {
        const got = fs.readSync(fd, buffer, read, length - read, at + read)
        if (got === 0) throw new Error(`${index.file}: legs[${leg}] ends before its ${length} bytes`)
        read += got
      }
      out.set(leg, buffer.toString("utf8"))
    }
  } finally {
    fs.closeSync(fd)
  }
  return out
}

/** Forget every index; a test's hook, so a rewritten scratch file is never read off the old one. */
export const forget = (): void => cache.clear()
