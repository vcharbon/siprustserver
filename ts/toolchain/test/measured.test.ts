/**
 * A command measured under a usage wrapper: its output lands in the log, the
 * figures come back from the usage file the wrapper wrote; a wrapper killed by
 * a signal answers 128 plus its number and no figures; a wrapper that cannot
 * start answers 127; and an interrupted measurement leaves no command running,
 * the wrapper still recording how it ended.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Fiber from "effect/Fiber"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as Liveness from "../src/liveness.js"
import * as Measured from "../src/measured.js"

const WRAPPER = path.join(import.meta.dirname, "fixtures", "stub-usage.sh")

let dir: string
beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "measured-test-"))
})
afterEach(() => {
  fs.rmSync(dir, { recursive: true, force: true })
})

const request = (command: string, args: ReadonlyArray<string>, wrapper = WRAPPER): Measured.Request => ({
  wrapper,
  command,
  args,
  cwd: dir,
  log: path.join(dir, "phase", "phase.log"),
  echo: false
})

const measure = (asked: Measured.Request) => Measured.run(asked).pipe(Effect.provide(NodeServices.layer))

describe("Measured.run", () => {
  it("keeps the output in the log and answers the figures the wrapper recorded", async () => {
    const measured = await Effect.runPromise(measure(request("sh", ["-c", "echo before; echo err >&2; exit 3"])))
    expect(measured).toMatchObject({ wallMs: 5, maxRssKb: 2048, exit: 3 })
    expect(measured.loadAtStart).toBeGreaterThanOrEqual(0)
    const log = fs.readFileSync(path.join(dir, "phase", "phase.log"), "utf8")
    expect(log).toContain("before")
    expect(log).toContain("err")
  })

  it("answers a wrapper killed by a signal with 128 plus the signal's number and no figures", async () => {
    const measured = await Effect.runPromise(measure(request("sh", ["-c", "kill -KILL $PPID; sleep 1"])))
    expect(measured.exit).toBe(128 + 9)
    expect(measured.maxRssKb).toBeNull()
  })

  it("answers a wrapper that cannot start with exit 127 and no peak", async () => {
    const measured = await Effect.runPromise(measure(request("true", [], path.join(dir, "no-such-wrapper"))))
    expect(measured.exit).toBe(127)
    expect(measured.maxRssKb).toBeNull()
  })

  it("stops the command when the measurement is interrupted, and the wrapper still records its exit", async () => {
    const pidFile = path.join(dir, "child.pid")
    const fiber = Effect.runFork(measure(request("sh", ["-c", `echo $$ > ${pidFile}; exec sleep 30`])))
    for (let i = 0; i < 100 && !fs.existsSync(pidFile); i++) await new Promise((r) => setTimeout(r, 50))
    const pid = Number(fs.readFileSync(pidFile, "utf8").trim())
    await Effect.runPromise(Fiber.interrupt(fiber))
    expect(await Effect.runPromise(Liveness.alive(pid))).toBe(false)
    const usage = JSON.parse(fs.readFileSync(path.join(dir, "phase", "phase.log.usage.json"), "utf8")) as { exit: number }
    expect(usage.exit).toBe(128 + 15)
  })
})
