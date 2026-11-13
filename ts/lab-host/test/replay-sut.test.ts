/**
 * A managed worker over a stand-in process: ready on its readiness line, and
 * never left running by a stand that did not hand it over — silent past its
 * readiness bound, exited first — so the lane's fixed port is free for the
 * next batch.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import type { LaneView } from "@sip/rig"
import * as Sut from "../src/replay-sut.js"

type SutPack = Sut.SutPack<LaneView.View, never>

let dir: string
beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "replay-sut-"))
})
afterEach(() => {
  fs.rmSync(dir, { recursive: true, force: true })
})

/** A stand-in worker: writes its pid to `pid`, then runs `script` in its own shell. */
const standIn = (script: string) =>
  ChildProcess.make("sh", ["-c", `echo $$ > ${path.join(dir, "pid")}; ${script}`], {
    stdin: "ignore",
    killSignal: "SIGTERM",
    forceKillAfter: "2 seconds"
  })

const run = <A, E>(effect: Effect.Effect<A, E, Sut.Requirements>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

/** Whether the stand-in's process is still there. */
const alive = (): boolean => {
  const pid = Number(fs.readFileSync(path.join(dir, "pid"), "utf8").trim())
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

describe("a managed worker", () => {
  it("is gone once its stand fails for a worker that never took its port", async () => {
    const log = path.join(dir, "sut.log")
    const failed = await run(Effect.flip(Sut.worker(standIn("echo starting; exec sleep 30"), log, "lane-x", "300 millis")))
    expect(String(failed)).toContain("never took its port")
    expect(alive()).toBe(false)
    expect(fs.readFileSync(log, "utf8")).toContain("starting")
  })

  it("is handed over once it prints its readiness line, and is gone once stopped", async () => {
    const log = path.join(dir, "sut.log")
    const held = await run(Sut.worker(standIn(`echo "${Sut.READY} 127.0.0.1:5060"; exec sleep 30`), log, "lane-x"))
    expect(held.died()).toBeUndefined()
    expect(alive()).toBe(true)
    await Effect.runPromise(held.stop)
    expect(alive()).toBe(false)
    expect(fs.readFileSync(log, "utf8")).toContain(Sut.READY)
  })

  it("fails its stand naming the exit of a worker that dies before it is ready", async () => {
    const failed = await run(Effect.flip(Sut.worker(standIn("exit 3"), path.join(dir, "sut.log"), "lane-x")))
    expect(String(failed)).toContain("exited 3 before it was ready")
  })

  it("keeps in its log what a stopped worker says on its way out", async () => {
    for (let attempt = 0; attempt < 3; attempt++) {
      const log = path.join(dir, `stop-${attempt}.log`)
      const held = await run(Sut.worker(
        standIn(`trap 'echo LEAVING >&2; exit 0' TERM; echo "${Sut.READY}"; while true; do sleep 0.05; done`),
        log,
        "lane-x"
      ))
      await Effect.runPromise(held.stop)
      expect(fs.readFileSync(log, "utf8"), `attempt ${attempt}`).toContain("LEAVING")
    }
  })

  it("keeps in its log the last line of a worker that dies before it is ready", async () => {
    for (let attempt = 0; attempt < 20; attempt++) {
      const log = path.join(dir, `dying-${attempt}.log`)
      const failed = await run(Effect.flip(Sut.worker(standIn("echo ADDRESS-IN-USE >&2; exit 3"), log, "lane-x")))
      expect(String(failed)).toContain("exited 3")
      expect(fs.readFileSync(log, "utf8"), `attempt ${attempt}`).toContain("ADDRESS-IN-USE")
    }
  })

  it("says how it died once it dies after it was handed over", async () => {
    const held = await run(Sut.worker(standIn(`echo "${Sut.READY}"; sleep 0.2; exit 7`), path.join(dir, "sut.log"), "lane-x"))
    await new Promise((resolve) => setTimeout(resolve, 600))
    expect(held.died()).toContain("the lane-x worker exited 7")
    await Effect.runPromise(held.stop)
  })
})

describe("a managed lane's worker", () => {
  const lane = { kind: "hosted", id: "lane-x", sip: { lifecycle: "managed", ingress: "127.0.0.1:5060" } } as unknown as LaneView.Hosted
  const packLogging = (logOf: SutPack["logOf"]): SutPack => ({
    worker: "stand-in",
    metricsOf: () => undefined,
    ...(logOf === undefined ? {} : { logOf }),
    command: () => Effect.succeed(standIn(`echo "${Sut.READY}"; echo "a call came"; exec sleep 30`))
  })

  it("logs to the file its pack states for the lane, its directory made", async () => {
    const log = path.join(dir, "named", "sut.log")
    const held = await run(Sut.stand(packLogging(() => log), lane, path.join(dir, "batch")))
    await new Promise((resolve) => setTimeout(resolve, 200))
    await Effect.runPromise(held!.stop)
    expect(fs.readFileSync(log, "utf8")).toContain("a call came")
    expect(fs.existsSync(path.join(dir, "batch"))).toBe(false)
  })

  it("logs under the batch's directory where its pack states no file", async () => {
    const held = await run(Sut.stand(packLogging(undefined), lane, path.join(dir, "batch")))
    await Effect.runPromise(held!.stop)
    expect(fs.readdirSync(path.join(dir, "batch")).filter((name) => name.startsWith("sut-"))).toHaveLength(1)
  })

  it("names its process in the pid file its pack states for the lane while it runs, the file gone once it stopped", async () => {
    const pidFile = path.join(dir, "named", "sut.pid")
    const held = await run(Sut.stand({ ...packLogging(undefined), pidFileOf: () => pidFile }, lane, path.join(dir, "batch")))
    expect(fs.readFileSync(pidFile, "utf8").trim()).toBe(fs.readFileSync(path.join(dir, "pid"), "utf8").trim())
    await Effect.runPromise(held!.stop)
    expect(alive()).toBe(false)
    expect(fs.existsSync(pidFile)).toBe(false)
  })

  /** The managed lane's worker named in `pidFile`, and how to kill the one running and wait for its end. */
  const named = async (pidFile: string) => {
    const held = (await run(Sut.stand({ ...packLogging(undefined), pidFileOf: () => pidFile }, lane, path.join(dir, "batch"))))!
    const current = () => Number(fs.readFileSync(pidFile, "utf8").trim())
    const killed = async () => {
      const pid = current()
      process.kill(pid, "SIGKILL")
      while (alive()) await new Promise((resolve) => setTimeout(resolve, 10))
      return pid
    }
    return { held, current, killed }
  }

  it("stands its worker again where it died, named in its pid file, and leaves a running one as it is", async () => {
    const pidFile = path.join(dir, "named", "sut.pid")
    const { held, current, killed } = await named(pidFile)
    const first = current()
    expect(await run(held.restand!(first)), "a running worker left as it is").toBeUndefined()
    expect(current()).toBe(first)
    await killed()
    const restood = await run(held.restand!(first))
    expect(restood).toEqual({ exit: expect.stringContaining("the lane-x worker exited SIGKILL"), claimed: true })
    expect(current()).not.toBe(first)
    expect(alive(), "the worker stood again runs").toBe(true)
    expect(held.died(), "a death the step claimed is no death of the lane's system").toBeUndefined()
    await Effect.runPromise(held.stop)
    expect(alive()).toBe(false)
    expect(fs.existsSync(pidFile)).toBe(false)
  })

  it("keeps saying a death no step claimed once it stood its worker again, beside a later one", async () => {
    const pidFile = path.join(dir, "named", "sut.pid")
    const { held, current, killed } = await named(pidFile)
    const first = await killed()
    expect(await run(held.restand!(first + 1))).toEqual({ exit: expect.stringContaining("exited SIGKILL"), claimed: false })
    expect(held.died(), "the death of a worker the step did not stop").toContain("the lane-x worker exited SIGKILL")
    expect(alive(), "the worker stood again runs").toBe(true)
    const second = await killed()
    expect(await run(held.restand!())).toEqual({ exit: expect.stringContaining("exited SIGKILL"), claimed: false })
    expect(held.died()!.split("; "), "each death no step claimed").toHaveLength(2)
    expect(current()).not.toBe(second)
    await Effect.runPromise(held.stop)
    expect(held.died()!.split("; ")).toHaveLength(2)
  })
})
