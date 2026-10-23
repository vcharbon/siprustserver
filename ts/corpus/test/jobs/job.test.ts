/**
 * A job's process, launched detached, answered for from its record: a job
 * whose process is killed reads `died` and its open runs are reaped; one
 * whose process exited on its own reads the exit its supervisor wrote; a
 * running one is left alone.
 */
import { NodeServices } from "@effect/platform-node"
import { Jobs, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import { execFileSync, spawn } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as Job from "../../src/jobs/job.js"
import * as JobStart from "../../src/jobs/job-start.js"
import * as Reap from "../../src/housekeeping/reap.js"

let tree: string

beforeEach(() => {
  tree = fs.mkdtempSync(path.join(os.tmpdir(), "job-test-"))
})
afterEach(() => {
  fs.rmSync(tree, { recursive: true, force: true })
})

const refuse = Effect.die("not in this test")

/** An answer read off the record and the kernel. */
const live = <A, E>(effect: Effect.Effect<A, E, FileSystem.FileSystem>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

const row = (id: number, launched: Partial<Jobs.Row>): Jobs.Row => ({
  id,
  kind: "replay",
  requestId: 1,
  lane: "paused",
  args: {},
  status: "running",
  createdAt: "2026-09-20T00:00:00.000Z",
  createdBy: "test",
  pid: null,
  startedAt: "2026-09-20T00:00:00.000Z",
  finishedAt: null,
  exit: null,
  logPath: null,
  hooksPath: null,
  waitingOn: null,
  sweepId: null,
  batchAt: null,
  batches: null,
  stopRequestedAt: null,
  stopGrace: null,
  restartOf: null,
  planned: null,
  ...launched
})

const until = async (ready: () => Promise<boolean>, ms = 5_000): Promise<void> => {
  const deadline = Date.now() + ms
  while (!(await ready())) {
    if (Date.now() > deadline) throw new Error("timed out")
    await new Promise((resolve) => setTimeout(resolve, 50))
  }
}

const launch = (id: number, argv: ReadonlyArray<string>) =>
  Effect.runPromise(Job.launch(tree, id, argv, tree).pipe(Effect.provide(NodeServices.layer)))

/**
 * A launch on a box slower than the test's own: each named command the
 * launcher and its supervisor look up on PATH runs as the shim `body(real)`
 * gives it, `real` being the command's own path.
 */
const launchSlowed = async (
  shims: Record<string, (real: string) => string>,
  id: number,
  argv: ReadonlyArray<string>
): Promise<Job.Launched> => {
  const dir = path.join(tree, "shims")
  fs.mkdirSync(dir, { recursive: true })
  for (const [name, body] of Object.entries(shims)) {
    const real = execFileSync("sh", ["-c", `command -v ${name}`], { encoding: "utf8" }).trim()
    fs.writeFileSync(path.join(dir, name), `#!/bin/sh\n${body(real)}\n`, { mode: 0o755 })
  }
  const pathBefore = process.env.PATH
  process.env.PATH = `${dir}:${pathBefore}`
  try {
    return await launch(id, argv)
  } finally {
    process.env.PATH = pathBefore
  }
}

/** A supervisor that lingers after it wrote its exit, as one descheduled between the two does. */
const lingeringExit = { mv: (real: string) => `${real} "$@"; status=$?; sleep 0.3; exit $status` }

/** Whether `pid` leads a process group: a signal to `-pid` reaches someone. */
const groupLeads = (pid: number): boolean => {
  try {
    process.kill(-pid, 0)
    return true
  } catch {
    return false
  }
}

describe("a job's process", () => {
  it("is alive by its record while it runs, and died once its group is killed with no exit written", async () => {
    const launched = await launch(1, ["sleep", "30"])
    const running = row(1, launched)
    expect(fs.existsSync(launched.logPath)).toBe(true)
    expect(await live(Job.liveness(running))).toEqual({ _tag: "running" })
    // The whole group, supervisor included: nothing is left to write the exit.
    process.kill(-launched.pid, "SIGKILL")
    await until(async () => !(await live(Job.processAlive(running))))
    expect(await live(Job.liveness(running))).toEqual({ _tag: "ended", close: { status: "died", exit: null } })
  })

  it("is launched with its record whole: the log exists and the supervisor leads its group, however late it is scheduled", async () => {
    // A `setsid` slow on both sides of the session it makes: the supervisor
    // leads its group, then opens its log, each well after the launcher's own
    // shell could have returned.
    const launched = await launchSlowed(
      { setsid: (real) => `sleep 0.3\nexec ${real} /bin/sh -c 'sleep 0.3; exec "$@"' sh "$@"` },
      7,
      ["sleep", "30"]
    )
    try {
      expect(fs.existsSync(launched.logPath)).toBe(true)
      expect(groupLeads(launched.pid)).toBe(true)
    } finally {
      try {
        process.kill(-launched.pid, "SIGKILL")
      } catch {
        process.kill(launched.pid, "SIGKILL")
      }
    }
  })

  it("reads the exit the supervisor wrote when the process ended on its own", async () => {
    const launched = await launchSlowed(lingeringExit, 2, ["sh", "-c", "exit 3"])
    const ended = row(2, launched)
    await until(async () => !(await live(Job.processAlive(ended))))
    expect(await live(Job.liveness(ended))).toEqual({ _tag: "ended", close: { status: "failed", exit: 3 } })
  })

  it("is stopped as a group: the TERM reaches the command and the supervisor still writes its exit", async () => {
    const launched = await launchSlowed(lingeringExit, 6, ["sh", "-c", "trap 'exit 143' TERM; while :; do sleep 0.05; done"])
    const running = row(6, launched)
    await live(Job.stop(running))
    await until(async () => !(await live(Job.processAlive(running))))
    expect(await live(Job.liveness(running))).toEqual({ _tag: "ended", close: { status: "failed", exit: 143 } })
    // Stopping what is gone is refused, never guessed at.
    const refused = await live(Job.stop(running).pipe(Effect.catch((error) => Effect.succeed(error))))
    expect(refused).toBeInstanceOf(Job.NotStopped)
  })

  it("answers for a foreground job by its pid alone", async () => {
    const child = spawn("sleep", ["30"], { stdio: "ignore" })
    const foreground = row(3, { pid: child.pid! })
    expect(await live(Job.liveness(foreground))).toEqual({ _tag: "running" })
    child.kill("SIGKILL")
    await new Promise((resolve) => child.once("exit", resolve))
    expect(await live(Job.liveness(foreground))).toEqual({ _tag: "ended", close: { status: "died", exit: null } })
  })
})

describe("starting a replay job", () => {
  const request = {
    id: 11,
    lane: "real",
    selection: { kind: "captures", captures: ["trace.pcap.gz"] },
    knobs: {},
    createdAt: "2026-09-20T00:00:00.000Z",
    createdBy: "test",
    cells: 30,
    captures: 1,
    jobs: 0
  } as unknown as Parameters<typeof JobStart.replayRow>[0]

  it("plans the request's cells on the row from the start, and nothing for a resume", () => {
    expect(JobStart.replayRow(request, JobStart.replayKnobsOf({}))).toEqual({ requestId: 11, lane: "real", planned: 30 })
    expect(JobStart.replayRow(request, { ...JobStart.replayKnobsOf({}), unran: true }, 9)).toEqual({
      requestId: 11,
      lane: "real",
      restartOf: 9
    })
  })

  it("passes on the knobs the start stated and leaves the rest for the replay to resolve", () => {
    const knobs = { ...JobStart.replayKnobsOf({}), concurrency: 8, startsPerSecond: 0 }
    const args = JobStart.replayArgs(5, 11, knobs)
    expect(args).toEqual(["--request", "11", "--job", "5", "--concurrency", "8", "--starts-per-second", "0", "--cleanup"])
  })

  it("reads a knob a row's args leave out as unstated, one it carries as stated", () => {
    const read = JobStart.replayKnobsOf({ batch: 1000, memoryMax: "2G" })
    expect([read.batch, read.concurrency, read.memoryMax, read.startsPerSecond]).toEqual([1000, null, "2G", null])
  })
})

describe("reaping jobs", () => {
  it("closes a died job and the runs it left open, and leaves a running one alone", async () => {
    const dead = await launch(4, ["sleep", "30"])
    process.kill(-dead.pid, "SIGKILL")
    const alive = await launch(5, ["sleep", "30"])
    const died = row(4, dead)
    const running = row(5, alive)
    await until(async () => !(await live(Job.processAlive(died))))

    const closed: Array<[number, Jobs.CloseRequest]> = []
    const abandoned: Array<[ReadonlyArray<number>, string]> = []
    const store = Layer.mergeAll(
      Layer.succeed(
        Jobs.Service,
        Jobs.Service.of({
          create: () => refuse,
          started: () => refuse,
          close: (id, request) => Effect.sync(() => void closed.push([id, request])),
          waiting: () => refuse,
          batch: () => refuse,
          sweep: () => refuse,
          stop: () => refuse,
          planned: () => refuse,
          settle: () => refuse,
          show: () => refuse,
          explain: () => refuse,
          list: () => refuse,
          page: () => refuse,
          lineage: () => refuse,
          gone: () => refuse,
          red: () => refuse,
          running: Effect.succeed([died, running])
        })
      ),
      Layer.succeed(
        Runs.Service,
        Runs.Service.of({
          open: () => refuse,
          openMany: () => refuse,
          finish: () => refuse,
          importRun: () => refuse,
          lanes: refuse,
          ranOn: () => refuse,
          standings: () => refuse,
          standing: () => refuse,
          protectedBundles: refuse,
          allBundles: refuse,
          unfinished: refuse,
          abandonable: () => refuse,
          abandon: (ids, detail) =>
            Effect.sync(() => {
              abandoned.push([ids, detail])
              return ids.length
            }),
          unfinishedOf: (ids) => Effect.succeed(ids.includes(4) ? [7, 8] : []),
          dirty: () => refuse,
          openOfSweep: () => refuse,
          forget: () => refuse
        })
      )
    )
    try {
      const reaped = await Effect.runPromise(Reap.reapJobs(false).pipe(Effect.provide(Layer.merge(store, NodeServices.layer))))
      expect(reaped).toEqual([{ id: 4, close: { status: "died", exit: null }, runs: [7, 8] }])
      expect(closed).toEqual([[4, { status: "died", exit: null }]])
      expect(abandoned).toEqual([[[7, 8], Reap.JOB_DETAIL]])
      expect(Reap.jobLines(reaped, false)).toEqual([
        "[cleanup] job 4 reads died: its process is gone; 2 open run(s) closed"
      ])
      // A dry run rules the same and closes nothing.
      closed.length = 0
      abandoned.length = 0
      expect(await Effect.runPromise(Reap.reapJobs(true).pipe(Effect.provide(Layer.merge(store, NodeServices.layer))))).toHaveLength(1)
      expect(closed).toEqual([])
      expect(abandoned).toEqual([])
    } finally {
      process.kill(-alive.pid, "SIGKILL")
    }
  })
})
