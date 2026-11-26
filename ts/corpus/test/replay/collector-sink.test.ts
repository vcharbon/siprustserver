/**
 * The run's output of a scope's collector events (`collector-io.ts`
 * `sinkOf`): one line per event, each step's end with the time it took on the
 * events' clock since that collector's start of it, two collectors timed
 * apart; an event ending no started step stated alone.
 */
import type { CollectorRun } from "@sip/rig"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import * as CollectorIo from "../../src/replay/collector-io.js"

const event = (collector: string, phase: CollectorRun.Phase, clock_ms: number, message?: string): CollectorRun.Event => ({
  collector,
  phase,
  clock_ms,
  wall_ms: 1_700_000_000_000 + clock_ms,
  ...(message === undefined ? {} : { message })
})

/** The lines `events`, through one sink, printed. */
const printed = (events: ReadonlyArray<CollectorRun.Event>) => {
  const lines: Array<string> = []
  const heard: Console.Console = { ...globalThis.console, log: (...args: ReadonlyArray<unknown>) => void lines.push(args.map(String).join(" ")) }
  const sink = CollectorIo.sinkOf(undefined)
  Effect.runSync(Effect.forEach(events, sink, { discard: true }).pipe(Effect.provideService(Console.Console, heard)))
  return lines
}

describe("a scope's collector events on the run's output", () => {
  it("state each step's end with the time it took, two collectors timed apart", () => {
    expect(printed([
      event("db", "open", 0),
      event("cdr", "collect", 100),
      event("db", "opened", 340),
      event("db", "collect", 400),
      event("cdr", "failed", 12_400, "collect timed out after 12000 ms"),
      event("db", "collected", 2_500)
    ])).toEqual([
      "[collector] db: open",
      "[collector] cdr: collect",
      "[collector] db: opened (340 ms)",
      "[collector] db: collect",
      "[collector] cdr: failed (12.3 s): collect timed out after 12000 ms",
      "[collector] db: collected (2.1 s)"
    ])
  })

  it("state an event that ends no started step without a time", () => {
    expect(printed([event("db", "failed", 50, "its locks not taken: held"), event("db", "lab-left-changed", 60)])).toEqual([
      "[collector] db: failed: its locks not taken: held",
      "[collector] db: lab-left-changed"
    ])
  })
})
