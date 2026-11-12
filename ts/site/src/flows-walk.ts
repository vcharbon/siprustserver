/**
 * A flows document read off a FILE by WALKING it: its envelope, how many legs it
 * holds, and a window of those legs — never the document itself. A capture of
 * thousands of calls has a document over V8's 512 MiB string cap, so nothing
 * here reads or parses more than one leg at a time.
 *
 * One walk answers all three. `@sip/pipeline`'s segments give the envelope's two
 * halves and where each leg's own text lies, and a window is then read at those
 * places, so a page load walks the file once and every window after it costs its
 * own legs. Reading is synchronous: a walk holds the loop, about six seconds per
 * 600 MiB.
 */
import { FlowsSegments } from "@sip/pipeline"
import { Buffer } from "node:buffer"
import * as fs from "node:fs"
import { make as makeMemo } from "./memo.ts"

/** How many walked files are remembered: an entry is an envelope and two numbers a leg. */
const WALKS = 4

/** What one walk of a file answers, for as long as that file stands unchanged. */
export interface Walked {
  /** How many legs the document holds. */
  readonly legs: number
  /** The document with its `legs` emptied. Parsed per call: the caller owns it. */
  readonly envelope: () => unknown
  /** `count` legs from `from`, at the document's own indices, each parsed alone. */
  readonly window: (from: number, count: number) => ReadonlyArray<unknown>
}

export interface FlowsWalks {
  /**
   * The walk of `file`, remembered under the file's identity. `null` where it
   * is unreadable or is not a flows document — one with no top-level `legs`, or
   * whose envelope does not parse, is not a document of no calls. A refusal is
   * remembered too: a file that is not a document must not be scanned again per
   * request.
   */
  readonly walk: (file: string) => Walked | null
}

const decode = (bytes: Uint8Array): string => Buffer.from(bytes).toString("utf8")

/** `length` bytes of `fd` from `at`, however many reads that takes. */
const readAt = (fd: number, at: number, length: number): Buffer => {
  const bytes = Buffer.allocUnsafe(length)
  let read = 0
  while (read < length) {
    const got = fs.readSync(fd, bytes, read, length - read, at + read)
    if (got === 0) break
    read += got
  }
  return read === length ? bytes : bytes.subarray(0, read)
}

/** The envelope's text and where every leg lies, off one pass over the file. */
const walkFile = (file: string): Walked => {
  let head = ""
  let tail = ""
  // Two numbers per leg — where its own text starts, and how long it is. Both
  // are BYTES, which is what a leg is read back by.
  const places: Array<number> = []
  let at = 0
  for (const segment of FlowsSegments.segments(file)) {
    if (segment.kind === "leg") places.push(at, segment.json)
    else if (segment.kind === "head") head = decode(segment.bytes)
    else tail = decode(segment.bytes)
    at += segment.bytes.length
  }
  // The head ends on the `[` of `legs` and the tail opens on its `]`, so the two
  // parse as the document with no leg in it — which is checked here, so that a
  // file this answers for is one every route can serve.
  const envelope = head + tail
  JSON.parse(envelope)
  const legs = places.length / 2
  return {
    legs,
    envelope: () => JSON.parse(envelope) as unknown,
    window: (from, count) => {
      const first = Math.min(Math.max(0, Math.trunc(from)), legs)
      const last = Math.min(first + Math.max(0, Math.trunc(count)), legs)
      if (last <= first) return []
      const fd = fs.openSync(file, "r")
      try {
        const window: Array<unknown> = []
        for (let index = first; index < last; index++) {
          window.push(JSON.parse(readAt(fd, places[index * 2]!, places[index * 2 + 1]!).toString("utf8")))
        }
        return window
      } finally {
        fs.closeSync(fd)
      }
    }
  }
}

/** A reader of flows files remembering the {@link WALKS} files last walked. */
export const make = (): FlowsWalks => {
  const walked = makeMemo<Walked | null>(WALKS)
  return {
    walk: (file) => {
      let stat: fs.Stats
      try {
        stat = fs.statSync(file)
      } catch {
        return null
      }
      // What a walk is true of: a file whose mtime or size moved is another one.
      const key = JSON.stringify([file, stat.mtimeMs, stat.size])
      const held = walked.get(key)
      if (held !== undefined) return held
      let fresh: Walked | null
      try {
        fresh = walkFile(file)
      } catch {
        fresh = null
      }
      walked.put(key, fresh)
      return fresh
    }
  }
}
