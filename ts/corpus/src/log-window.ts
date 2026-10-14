/**
 * A log cut by wall-clock window. A line states its instant at its head —
 * `<service>  | <RFC 3339>` as `docker compose logs --timestamps` writes it,
 * or `<RFC 3339>` as `docker logs -t` does — and a line stating none
 * continues the one above it, so a log's first lines before any stamp fall in
 * no window. One container's log is in time order: a file is cut by bisection
 * on byte offsets, reading a few lines per probe, never the file.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"

/** A wall-clock window, Unix milliseconds, both bounds included. */
export interface Window {
  readonly fromMs: number
  readonly toMs: number
}

/**
 * How far a window reaches past the cell's own bounds: a SUT logs a call's
 * first message as it arrives and its teardown a moment after the last.
 */
export const PAD_MS = 1_000

/** `window` reaching {@link PAD_MS} further on each side. */
export const padded = (window: Window): Window => ({ fromMs: window.fromMs - PAD_MS, toMs: window.toMs + PAD_MS })

const HEAD = /^(?:[^|]*\|\s)?\s*(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d))\s/

/** The instant a line states at its head, Unix milliseconds; `undefined` where it states none. */
export const stampOf = (line: string): number | undefined => {
  const match = HEAD.exec(line)
  if (match === null) return undefined
  const ms = Date.parse(match[1]!)
  return Number.isNaN(ms) ? undefined : ms
}

/** How much of the file one read takes. */
const CHUNK = 64 * 1024

/** A log file open for cutting. */
interface Opened {
  readonly file: FileSystem.File
  readonly size: number
}

const decoder = new TextDecoder()

/** The line starting at byte `start`, without its newline, and where the next one starts. */
const lineAt = Effect.fn("LogWindow.lineAt")(function*(opened: Opened, start: number) {
  const parts: Array<Uint8Array> = []
  let at = start
  while (at < opened.size) {
    yield* opened.file.seek(at, "start")
    const read = yield* opened.file.readAlloc(Math.min(CHUNK, opened.size - at))
    if (Option.isNone(read) || read.value.length === 0) break
    const chunk = read.value
    const newline = chunk.indexOf(10)
    if (newline >= 0) {
      parts.push(chunk.subarray(0, newline))
      return { text: decoder.decode(Buffer.concat(parts)), next: at + newline + 1 }
    }
    parts.push(chunk)
    at += chunk.length
  }
  return { text: decoder.decode(Buffer.concat(parts)), next: opened.size }
})

/** Where the first line starting at or after byte `at` starts. */
const lineStartFrom = (opened: Opened, at: number) =>
  at === 0 ? Effect.succeed(0) : Effect.map(lineAt(opened, at - 1), (line) => line.next)

/** The first stamped line starting at or after byte `at`: its start and its instant. */
const stampedFrom = Effect.fn("LogWindow.stampedFrom")(function*(opened: Opened, at: number) {
  let start = yield* lineStartFrom(opened, at)
  while (start < opened.size) {
    const line = yield* lineAt(opened, start)
    const stamp = stampOf(line.text)
    if (stamp !== undefined) return { start, stamp }
    start = line.next
  }
  return undefined
})

const cutOpen = Effect.fn("LogWindow.cutOpen")(function*(opened: Opened, window: Window) {
  // The least offset whose first stamped line is in or past the window.
  let low = 0
  let high = opened.size
  while (low < high) {
    const mid = Math.floor((low + high) / 2)
    const probe = yield* stampedFrom(opened, mid)
    if (probe === undefined || probe.stamp >= window.fromMs) high = mid
    else low = mid + 1
  }
  const first = yield* stampedFrom(opened, low)
  if (first === undefined || first.stamp > window.toMs) return ""
  const lines: Array<string> = []
  let start = first.start
  while (start < opened.size) {
    const line = yield* lineAt(opened, start)
    const stamp = stampOf(line.text)
    if (stamp !== undefined && stamp > window.toMs) break
    lines.push(`${line.text}\n`)
    start = line.next
  }
  return lines.join("")
})

/** The lines of the log at `path` inside `window`, each ended by a newline; "" where none falls in it or it cannot be read. */
export const cutFile = (path: string, window: Window): Effect.Effect<string, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const file = yield* fs.open(path, { flag: "r" })
    const info = yield* file.stat
    return yield* cutOpen({ file, size: Number(info.size) }, window)
  }).pipe(Effect.scoped, Effect.orElseSucceed(() => ""))
