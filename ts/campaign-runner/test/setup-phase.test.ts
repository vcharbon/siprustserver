/**
 * A run's setup phase without a store: each event a line of the run's output,
 * and the operations that left the lab changed told once every heal ran,
 * after the rest of the run (its report) and however the run ended.
 */
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as fs from "node:fs"
import { afterEach, describe, expect, it } from "vitest"
import { leftChangedOf, lineOf, setupOf } from "../src/setup-phase.js"
import { planned, traceOf, workspace } from "./setup-operations.js"

const made: Array<string> = []
afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** The setup `names` on a fresh workspace, with what it told of a lab left changed. */
const setup = async (names: ReadonlyArray<string>) => {
  const root = workspace()
  made.push(root)
  const instances = await planned(root, names)
  const told: Array<ReadonlyArray<string>> = []
  const around = setupOf(instances, () => Effect.void, (left) => Effect.sync(() => void told.push(left)))
  return { root, told, around }
}

describe("a setup event as the run prints it", () => {
  it("names the operation, the phase and what failed", () => {
    expect(lineOf({ operation: "stuck", phase: "failed", clock_ms: 1, wall_ms: 1, message: "heal exited 1" }))
      .toBe("[setup] stuck: failed: heal exited 1")
    expect(lineOf({ operation: "restart-worker", phase: "injected", clock_ms: 1, wall_ms: 1 })).toBe("[setup] restart-worker: injected")
  })
})

describe("a lab left changed, as the run's record learns it", () => {
  it("is told after the report and every heal, naming each operation that did not heal", async () => {
    const { root, told, around } = await setup(["stuck", "restart-worker"])
    const healed = await Effect.runPromise(around(Effect.sync(() => fs.appendFileSync(`${root}/trace`, "report\n"))))
    expect(healed.labLeftChanged).toBe(true)
    expect(traceOf(root)).toEqual(["inject stuck", "inject restart-worker", "report", "heal restart-worker", "heal stuck", "heal stuck"])
    expect(told).toEqual([["stuck"]])
    expect(leftChangedOf(told[0]!)).toBe("setup: lab left changed by stuck: 2 heal attempts failed")
  })

  it("is told for a run that was stopped", async () => {
    const { root, told, around } = await setup(["stuck"])
    const exit = await Effect.runPromise(Effect.gen(function*() {
      const fiber = yield* Effect.forkChild(around(Effect.never))
      yield* Effect.sleep("20 millis").pipe(Effect.repeat({ until: () => traceOf(root).includes("inject stuck") }))
      return yield* Fiber.interrupt(fiber).pipe(Effect.andThen(Fiber.await(fiber)))
    }))
    expect(Exit.hasInterrupts(exit)).toBe(true)
    expect(told).toEqual([["stuck"]])
  })

  it("is not told where every heal recovered", async () => {
    const { told, around } = await setup(["restart-worker"])
    const healed = await Effect.runPromise(around(Effect.void))
    expect(healed.labLeftChanged).toBe(false)
    expect(told).toEqual([])
  })
})
