/**
 * A step's log relayed to the run's output (`step-relay.ts`): what the step
 * appends, while it runs and once it ended, its last line without a newline
 * included, never what the log held before; a step that fails still relayed
 * to its end; past its head, its tail at its end after one line naming the log. The relay's polls run on
 * a test clock the step itself moves.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as TestClock from "effect/testing/TestClock"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as StepRelay from "../src/step-relay.js"

const made: Array<string> = []
afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** A log file of the case's own holding `before`. */
const logWith = (before: string): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "step-relay-"))
  made.push(dir)
  const log = path.join(dir, "collect.log")
  fs.writeFileSync(log, before)
  return log
}

/** `effect` run with the run's output kept: what it ended with, and each line printed. */
const relaying = async <A, E>(effect: (heard: ReadonlyArray<string>) => Effect.Effect<A, E>) => {
  const lines: Array<string> = []
  const heard: Console.Console = { ...globalThis.console, log: (...args: ReadonlyArray<unknown>) => void lines.push(args.map(String).join(" ")) }
  const exit = await Effect.runPromiseExit(effect(lines).pipe(Effect.provideService(Console.Console, heard), Effect.provide(TestClock.layer())))
  return { exit, lines }
}

describe("a step's log relayed to the run's output", () => {
  it("prints each line the step appends while it runs and its unfinished last line at its end, nothing it held before", async () => {
    const log = logWith("an earlier attempt's line\n")
    let seenWhileRunning: ReadonlyArray<string> = []
    const { exit, lines } = await relaying((heard) =>
      StepRelay.relayed("records", "collect", log, Effect.gen(function*() {
        fs.appendFileSync(log, "the store as found\nhalf ")
        yield* TestClock.adjust(StepRelay.POLL_MS)
        seenWhileRunning = [...heard]
        fs.appendFileSync(log, "a line\nno newline")
        return "ended"
      }))
    )
    expect(exit._tag).toBe("Success")
    expect(seenWhileRunning).toEqual(["[collector] records (collect) the store as found"])
    expect(lines).toEqual([
      "[collector] records (collect) the store as found",
      "[collector] records (collect) half a line",
      "[collector] records (collect) no newline"
    ])
  })

  it("relays a failing step to its last line", async () => {
    const log = logWith("")
    const { exit, lines } = await relaying(() =>
      StepRelay.relayed("db", "open", log, Effect.andThen(Effect.sync(() => fs.appendFileSync(log, "dump refused: disk full\n")), Effect.fail("exited 1")))
    )
    expect(exit._tag).toBe("Failure")
    expect(lines).toEqual(["[collector] db (open) dump refused: disk full"])
  })

  it(`prints the first ${StepRelay.HEAD_LINES} lines of a step as they come and its last ${StepRelay.TAIL_LINES} at its end, after one counting those left out`, async () => {
    const log = logWith("")
    const total = StepRelay.HEAD_LINES + 80 + StepRelay.TAIL_LINES
    const { lines } = await relaying(() =>
      StepRelay.relayed("chatty", "collect", log, Effect.sync(() => fs.appendFileSync(log, Array.from({ length: total }, (_, i) => `line ${i}\n`).join(""))))
    )
    expect(lines).toHaveLength(StepRelay.HEAD_LINES + 1 + StepRelay.TAIL_LINES)
    expect(lines[StepRelay.HEAD_LINES - 1]).toBe(`[collector] chatty (collect) line ${StepRelay.HEAD_LINES - 1}`)
    expect(lines[StepRelay.HEAD_LINES]).toBe(`[collector] chatty (collect) … 80 line(s) left out, every line in ${log}`)
    expect(lines.at(-1)).toBe(`[collector] chatty (collect) line ${total - 1}`)
  })
})
