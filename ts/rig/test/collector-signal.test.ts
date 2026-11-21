/**
 * A runner process killed by a signal while a batch runs closes the batch's
 * collectors before it exits, in reverse order, and the process an `open`
 * left running is ended by its `close`. The runner is one real node process
 * (`fixtures/collector-runner/main.ts`) and the collectors real executables.
 */
import { type ChildProcess, spawn } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { alive, gone, SAMPLES, traceOf } from "./collectors.js"
import { tree } from "./tree.js"
import { exitOf, phaseSeen } from "./waits.js"

const RUNNER = path.join(import.meta.dirname, "fixtures", "collector-runner", "main.ts")
const PACKAGE = path.join(import.meta.dirname, "..")

const made: Array<string> = []
const running: Array<ChildProcess> = []

afterEach(() => {
  for (const child of running.splice(0)) child.kill("SIGKILL")
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** The phases the runner wrote to `file` so far, as `<phase> <collector>`. */
const phasesOf = (file: string): ReadonlyArray<string> =>
  fs.existsSync(file)
    ? fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "").map((line) => {
      const event = JSON.parse(line) as { phase: string; collector: string }
      return `${event.phase} ${event.collector}`
    })
    : []

describe("a runner killed by a signal during a batch", () => {
  it("closes the batch's collectors, ending what an open left running", async () => {
    const root = tree(SAMPLES)
    made.push(root)
    const events = path.join(root, "events.jsonl")
    const child = spawn(process.execPath, ["--import", "tsx", RUNNER, root, "tidy,leaves-process", events], {
      cwd: PACKAGE,
      stdio: "ignore"
    })
    running.push(child)
    const { ended, exited } = exitOf(child)
    await phaseSeen(() => phasesOf(events), "opened leaves-process", ended)
    const sleeper = Number(fs.readFileSync(path.join(root, "sleeper"), "utf8"))
    expect(alive(sleeper)).toBe(true)
    child.kill("SIGTERM")
    expect((await exited).code).toBe(130)
    expect(traceOf(root)).toEqual(["open tidy open", "open leaves-process", "close leaves-process", "close tidy token close"])
    expect(phasesOf(events).slice(-4)).toEqual(["close leaves-process", "closed leaves-process", "close tidy", "closed tidy"])
    await gone(sleeper)
  })
})
