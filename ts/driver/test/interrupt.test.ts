/**
 * A running cell, interrupted: the interpreter it started receives SIGTERM,
 * the cell leaves `error.txt` saying so where its bundle would have been, and
 * its fiber ends interrupted.
 */
import { ReplayCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { INTERRUPTED, runCell } from "../src/cells.js"
import { cellDir, ERROR_FILE } from "../src/layout.js"
import { fixture, pivotDocument, rig, runDir, stubTests } from "./harness.js"

const dirs: Array<string> = []
afterEach(() => {
  for (const d of dirs.splice(0)) fs.rmSync(d, { recursive: true, force: true })
})

/**
 * Resolves once `ready` holds, checking every 20 ms; rejects as soon as
 * `fiber` ended without it. The package's test timeout bounds the wait.
 */
const until = async <A, E>(ready: () => boolean, fiber: Fiber.Fiber<A, E>): Promise<void> => {
  let ended = false
  void Effect.runPromise(Fiber.await(fiber)).then(() => {
    ended = true
  })
  while (!ready() && !ended) await new Promise((resolve) => setTimeout(resolve, 20))
  if (!ready()) throw new Error("the cell ended before the stub reported")
}

describe("an interrupted cell", () => {
  it("sends its interpreter SIGTERM and leaves error.txt saying interrupted", async () => {
    const dir = runDir("interrupt")
    dirs.push(dir)
    const marks = path.join(dir, "marks")
    const cell = { kind: "pivot-replay", case: pivotDocument("transparent-defect.v3.json"), lane: "stub-lane" } as const
    const fiber = Effect.runFork(
      runCell(cell, { runDir: dir, tests: stubTests(0, true), mode: "record" }).pipe(
        Effect.provideService(ReplayCli.Environment, { STUB_MARKS: marks }),
        Effect.provide(rig({ replay: fixture("stub-replay-slow.sh") }))
      )
    )
    await until(() => fs.existsSync(path.join(marks, "started")), fiber)
    const exit = await Effect.runPromise(Effect.andThen(Fiber.interrupt(fiber), Fiber.await(fiber)))
    expect(Exit.isFailure(exit)).toBe(true)
    expect(fs.readFileSync(path.join(marks, "signal"), "utf8").trim()).toBe("TERM")
    expect(fs.readFileSync(path.join(dir, cellDir(cell), ERROR_FILE), "utf8").trim()).toBe(INTERRUPTED)
  })
})
