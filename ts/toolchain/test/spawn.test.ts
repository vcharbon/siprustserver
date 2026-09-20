/**
 * The subprocess boundary under interruption: a run whose fiber is interrupted
 * ends the process it started. SIGTERM first; where the caller stated a grace
 * and the process sits through SIGTERM, SIGKILL once the grace is over.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { runner } from "../src/spawn.js"
import { fixture } from "./harness.js"

const dirs: Array<string> = []
afterEach(() => {
  for (const d of dirs.splice(0)) fs.rmSync(d, { recursive: true, force: true })
})

const alive = (pid: number): boolean => {
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

/** Polls until `ready` holds, or fails after a second. */
const until = async (ready: () => boolean): Promise<void> => {
  const deadline = Date.now() + 5_000
  while (!ready()) {
    if (Date.now() > deadline) throw new Error("the stub never reported")
    await new Promise((resolve) => setTimeout(resolve, 20))
  }
}

describe("an interrupted run", () => {
  it("SIGKILLs a process that ignores SIGTERM once the stated grace is over", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sip-toolchain-spawn-"))
    dirs.push(dir)
    const pidFile = path.join(dir, "pid")
    const fiber = Effect.runFork(
      Effect.gen(function* () {
        const cli = yield* runner
        return yield* cli.all(fixture("stub-ignores-term.sh"), [], {
          env: { STUB_PID: pidFile },
          forceKillAfter: "300 millis"
        })
      }).pipe(Effect.provide(NodeServices.layer))
    )
    await until(() => fs.existsSync(pidFile))
    const pid = Number(fs.readFileSync(pidFile, "utf8").trim())
    expect(alive(pid)).toBe(true)
    const started = Date.now()
    const exit = await Effect.runPromise(Effect.andThen(Fiber.interrupt(fiber), Fiber.await(fiber)))
    expect(Exit.isFailure(exit)).toBe(true)
    // The interrupt returns once the process is gone: after the grace, not before.
    expect(Date.now() - started).toBeGreaterThanOrEqual(250)
    await until(() => !alive(pid))
  })
})
