/**
 * A runner process killed by a signal while its job runs heals its setup
 * before it exits: the signal interrupts the job, never the heal, and a
 * second signal sent while it heals changes nothing. The runner is one real
 * node process (`fixtures/setup-runner/main.ts`, tsx as its loader, so no
 * wrapper process stands between the signal and it) and the operations real
 * executables.
 */
import { type ChildProcess, spawn } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { SAMPLES, traceOf } from "./operations.js"
import { tree } from "./tree.js"
import { exitOf, phaseSeen } from "./waits.js"

const RUNNER = path.join(import.meta.dirname, "fixtures", "setup-runner", "main.ts")
const PACKAGE = path.join(import.meta.dirname, "..")

const made: Array<string> = []
const running: Array<ChildProcess> = []

afterEach(() => {
  for (const child of running.splice(0)) child.kill("SIGKILL")
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** The phases the runner wrote to `file` so far, as `<phase> <operation>`. */
const phasesOf = (file: string): ReadonlyArray<string> =>
  fs.existsSync(file)
    ? fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "").map((line) => {
      const event = JSON.parse(line) as { phase: string; operation: string }
      return `${event.phase} ${event.operation}`
    })
    : []

describe("a runner killed by a signal", () => {
  it.each(["SIGTERM", "SIGINT"] as const)("heals its setup on %s, a second signal during the heal ignored", async (signal) => {
    const root = tree(SAMPLES)
    made.push(root)
    const events = path.join(root, "events.jsonl")
    const child = spawn(process.execPath, ["--import", "tsx", RUNNER, root, "first,slow-heal", events], {
      cwd: PACKAGE,
      stdio: "ignore"
    })
    running.push(child)
    const { ended, exited } = exitOf(child)
    await phaseSeen(() => phasesOf(events), "injected slow-heal", ended)
    child.kill(signal)
    await phaseSeen(() => phasesOf(events), "heal slow-heal", ended)
    child.kill(signal)
    await exited
    expect(traceOf(root)).toEqual(["inject first", "inject slow-heal", "heal slow-heal", "healed slow-heal", "heal first"])
    expect(phasesOf(events).slice(-6)).toEqual([
      "heal slow-heal",
      "healed slow-heal",
      "recovered slow-heal",
      "heal first",
      "healed first",
      "recovered first"
    ])
  })

  it("heals its setup when its terminal hangs up: SIGHUP, its output's reader gone, a second SIGHUP ignored", async () => {
    const root = tree(SAMPLES)
    made.push(root)
    const events = path.join(root, "events.jsonl")
    const child = spawn(process.execPath, ["--import", "tsx", RUNNER, root, "first,slow-heal", events], {
      cwd: PACKAGE,
      stdio: ["ignore", "pipe", "pipe"]
    })
    running.push(child)
    child.stdout.resume()
    child.stderr.resume()
    const { ended, exited } = exitOf(child)
    await phaseSeen(() => phasesOf(events), "injected slow-heal", ended)
    child.stdout.destroy()
    child.stderr.destroy()
    child.kill("SIGHUP")
    await phaseSeen(() => phasesOf(events), "heal slow-heal", ended)
    child.kill("SIGHUP")
    expect(await exited).toEqual({ code: 130, signal: null })
    expect(traceOf(root)).toEqual(["inject first", "inject slow-heal", "heal slow-heal", "healed slow-heal", "heal first"])
    expect(phasesOf(events).slice(-6)).toEqual([
      "heal slow-heal",
      "healed slow-heal",
      "recovered slow-heal",
      "heal first",
      "healed first",
      "recovered first"
    ])
  })
})
