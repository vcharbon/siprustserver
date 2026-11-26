/**
 * A step's log relayed to the run's output while the step runs: each line its
 * executable appends, from the log's size at the start, printed as
 * `[collector] <label> (<step>) <line>` every {@link POLL_MS} and once more at
 * the end, a last line without its newline included. The first
 * {@link HEAD_LINES} lines of a step are printed as they come; past them the
 * last {@link TAIL_LINES} are kept and printed at the step's end, after one
 * line counting those left out and naming the log, which keeps every line:
 * the step's end, where it says why it ended, always reaches the output.
 * The log is the executable's own file (its stderr), read and never written.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Schedule from "effect/Schedule"
import * as fs from "node:fs"
import { StringDecoder } from "node:string_decoder"

/** How often the log is read while the step runs. */
export const POLL_MS = 250

/** The lines of a step printed as they come. */
export const HEAD_LINES = 150

/** The last lines of a step past its head, printed at its end. */
export const TAIL_LINES = 50

/** `line` of `label`'s `step` as the run's output states it. */
export const lineOf = (label: string, step: string, line: string): string => `[collector] ${label} (${step}) ${line}`

/** The size of `file`, 0 where it does not exist yet. */
const sizeOf = (file: string): number => {
  try {
    return fs.statSync(file).size
  } catch {
    return 0
  }
}

/** What `file` holds past `offset`, or nothing where it cannot be read. */
const readFrom = (file: string, offset: number): Buffer => {
  try {
    const fd = fs.openSync(file, "r")
    try {
      const size = fs.fstatSync(fd).size
      if (size <= offset) return Buffer.alloc(0)
      const buffer = Buffer.alloc(size - offset)
      const read = fs.readSync(fd, buffer, 0, buffer.length, offset)
      return buffer.subarray(0, read)
    } finally {
      fs.closeSync(fd)
    }
  } catch {
    return Buffer.alloc(0)
  }
}

/** The relay of `log` for `label`'s `step`: `drain` prints what came since its last read, `final` its unfinished last line and its kept tail too. */
const relayOf = (label: string, step: string, log: string) => {
  let offset = sizeOf(log)
  let pending = ""
  let said = 0
  let skipped = 0
  const tail: Array<string> = []
  const decoder = new StringDecoder("utf8")
  const print = (line: string) => Console.log(lineOf(label, step, line))
  const take = (line: string) =>
    Effect.suspend(() => {
      if (said < HEAD_LINES) {
        said += 1
        return print(line)
      }
      tail.push(line)
      if (tail.length > TAIL_LINES) {
        tail.shift()
        skipped += 1
      }
      return Effect.void
    })
  const ending = Effect.suspend(() => {
    const kept = tail.splice(0)
    const gap = skipped === 0 ? [] : [`… ${skipped} line(s) left out, every line in ${log}`]
    return Effect.forEach([...gap, ...kept], print, { discard: true })
  })
  const drain = (final: boolean) =>
    Effect.suspend(() => {
      const chunk = readFrom(log, offset)
      offset += chunk.length
      const lines = (pending + decoder.write(chunk) + (final ? decoder.end() : "")).split("\n")
      pending = lines.pop() ?? ""
      if (final && pending !== "") {
        lines.push(pending)
        pending = ""
      }
      const taken = Effect.forEach(lines.filter((line) => line.trim() !== ""), take, { discard: true })
      return final ? Effect.andThen(taken, ending) : taken
    })
  return { drain }
}

/** `step` of `label` run with `log` relayed while it runs, and to its last line once it ended, however it ended. */
export const relayed = <A, E, R>(label: string, step: string, log: string, run: Effect.Effect<A, E, R>): Effect.Effect<A, E, R> =>
  Effect.suspend(() => {
    const relay = relayOf(label, step, log)
    return Effect.scoped(
      Effect.andThen(
        Effect.forkScoped(Effect.repeat(Effect.andThen(Effect.sleep(POLL_MS), relay.drain(false)), Schedule.forever)),
        run
      )
    ).pipe(Effect.ensuring(relay.drain(true)))
  })
