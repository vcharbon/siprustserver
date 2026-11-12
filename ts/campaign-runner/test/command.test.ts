/**
 * A run verb's exit code and the line it says on stderr, starting with the
 * pack's verb, where another run holds the environment or the run crashed,
 * and the load a lane's run row states off its pacing.
 */
import { LaneView } from "@sip/rig"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { exitCodeOf, loadOf } from "../src/command.js"
import { RunHeld } from "../src/exclusive.js"

const VERB = "rig run"

/** The exit code `program` ends with, and every line it wrote on stderr. */
const exitOf = async (program: Effect.Effect<0 | 1 | 2, unknown>) => {
  const stderr: Array<string> = []
  const console = { ...globalThis.console, error: (...args: ReadonlyArray<unknown>) => void stderr.push(args.join(" ")) }
  const code = await Effect.runPromise(
    exitCodeOf(VERB, program).pipe(Effect.provideService(Console.Console, console as unknown as Console.Console))
  )
  return { code, stderr }
}

describe("a run verb's exit code", () => {
  it("is the run's own where it ends, nothing said", async () => {
    expect(await exitOf(Effect.succeed(1 as const))).toEqual({ code: 1, stderr: [] })
  })

  it("is 2 where another run holds the environment, the holding run named on stderr", async () => {
    expect(await exitOf(Effect.fail(new RunHeld({ verb: VERB, env: "e1" })))).toEqual({
      code: 2,
      stderr: ["rig run: another rig run of e1 is running: one run per environment at a time"]
    })
  })

  it("reads a held environment by its tag, whichever copy of the class failed", async () => {
    const elsewhere = { _tag: "CampaignRunner.RunHeld", message: "another rig run of e2 is running" }
    expect(await exitOf(Effect.fail(elsewhere))).toEqual({ code: 2, stderr: ["rig run: another rig run of e2 is running"] })
  })

  it("is 2 where the run fails or dies, said as a crash", async () => {
    const failed = await exitOf(Effect.fail(new Error("store down")))
    expect(failed.code).toBe(2)
    expect(failed.stderr[0]).toMatch(/^rig run: crashed: Error: store down/)
    const died = await exitOf(Effect.die("boom"))
    expect(died.code).toBe(2)
    expect(died.stderr[0]).toMatch(/^rig run: crashed: .*boom/)
  })
})

describe("a lane's run row load", () => {
  it("states the simulated lane's pacing, no start rate, and one cell's memory box", () => {
    expect(loadOf(LaneView.simulatedBuiltIn("sim", {}))).toEqual({
      concurrency: 4,
      batchSize: 0,
      startsPerSecond: 0,
      maxSpanSeconds: null,
      memoryMax: "1G"
    })
  })
})
