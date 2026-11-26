/**
 * A step's log relayed to the run's output while the step runs: each line its
 * executable appends, from the log's size at the start, printed as
 * `[collector] <label> | <line>` every {@link POLL_MS} and once more at the
 * end, a last line without its newline included. At most {@link MAX_LINES}
 * lines a step: past them one line names the log, which keeps every line.
 * The log is the executable's own file (its stderr), read and never written.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Schedule from "effect/Schedule"
import * as fs from "node:fs"
import { StringDecoder } from "node:string_decoder"

/** How often the log is read while the step runs. */
export const POLL_MS = 250

/** The lines of one step relayed at most. */
export const MAX_LINES = 200

/** `line` of `label`'s step as the run's output states it. */
export const lineOf = (label: string, line: string): string => `[collector] ${label} | ${line}`

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

/** The relay of `log` for `label`: `drain` prints what came since its last read, `final` its unfinished last line too. */
const relayOf = (label: string, log: string) => {
  let offset = sizeOf(log)
  let pending = ""
  let said = 0
  const decoder = new StringDecoder("utf8")
  const say = (line: string) =>
    Effect.suspend(() => {
      said += 1
      if (said <= MAX_LINES) return Console.log(lineOf(label, line))
      if (said === MAX_LINES + 1) return Console.log(lineOf(label, `… further lines in ${log}`))
      return Effect.void
    })
  const drain = (final: boolean) =>
    Effect.suspend(() => {
      if (said > MAX_LINES) return Effect.void
      const chunk = readFrom(log, offset)
      offset += chunk.length
      const lines = (pending + decoder.write(chunk) + (final ? decoder.end() : "")).split("\n")
      pending = lines.pop() ?? ""
      if (final && pending !== "") {
        lines.push(pending)
        pending = ""
      }
      return Effect.forEach(lines.filter((line) => line.trim() !== ""), say, { discard: true })
    })
  return { drain }
}

/** `step` run with `log` relayed while it runs, and to its last line once it ended, however it ended. */
export const relayed = <A, E, R>(label: string, log: string, step: Effect.Effect<A, E, R>): Effect.Effect<A, E, R> =>
  Effect.suspend(() => {
    const relay = relayOf(label, log)
    return Effect.scoped(
      Effect.andThen(
        Effect.forkScoped(Effect.repeat(Effect.andThen(Effect.sleep(POLL_MS), relay.drain(false)), Schedule.forever)),
        step
      )
    ).pipe(Effect.ensuring(relay.drain(true)))
  })
