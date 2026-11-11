/**
 * A job's setup operations, run as real executables: injected in order, then
 * the job, then healed in reverse whatever ended the job (a failure, an
 * interruption, a failed inject); a heal that fails twice is the event
 * `lab-left-changed`; a probe answering 75 is called again within its bound;
 * each step is an event. The plan refuses a name the path does not define and
 * one whose parts no lane names.
 */
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import type * as Lanes from "../src/lanes.js"
import type * as OperationRun from "../src/operation-run.js"
import * as RigPath from "../src/rig-path.js"
import * as Setup from "../src/setup.js"
import { laneOf, SAMPLES, traceOf } from "./operations.js"
import { run, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** A rig path holding every sample operation, loaded; its root. */
const samples = async () => {
  const root = tree(SAMPLES)
  made.push(root)
  const loaded = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(loaded)) throw new Error(`the samples do not load: ${String(loaded.failure)}`)
  return { root, rig: loaded.success }
}

/** The setup `names` planned over `lanes` under `root`, which must hold. */
const plan = (rig: RigPath.RigPath, root: string, names: ReadonlyArray<string>, lanes: ReadonlyArray<Lanes.Loaded> = []) => {
  const planned = Setup.planOf(rig, "rig.toml", names, lanes, path.join(root, "job"))
  if (!Result.isSuccess(planned)) throw new Error(planned.failure.message)
  return planned.success
}

/** A sink collecting each event, and the phases it saw as `<phase> <operation>`. */
const collecting = () => {
  const events: Array<OperationRun.Event> = []
  const sink: OperationRun.Sink = (event) => Effect.sync(() => void events.push(event))
  return { events, sink, phases: () => events.map((event) => `${event.phase} ${event.operation}`) }
}

const exitOf = <A, E>(effect: Effect.Effect<A, E>) => Effect.runPromise(Effect.exit(effect))

describe("a job's setup", () => {
  it("injects in order, runs the job, heals in reverse, each step an event on both clocks", async () => {
    const { root, rig } = await samples()
    const { events, sink, phases } = collecting()
    const exit = await exitOf(Setup.around(plan(rig, root, ["first", "second"]), sink, Effect.sync(() => traceOf(root).length)))
    expect(exit).toEqual(Exit.succeed({ value: 2, labLeftChanged: false }))
    expect(traceOf(root)).toEqual(["inject first", "inject second", "heal second", "heal first"])
    expect(phases()).toEqual([
      "inject first",
      "injected first",
      "inject second",
      "injected second",
      "heal second",
      "healed second",
      "recovered second",
      "heal first",
      "healed first",
      "recovered first"
    ])
    for (const event of events) {
      expect(event.wall_ms).toBeGreaterThan(Date.parse("2020-01-01"))
      expect(event.clock_ms).toBeGreaterThan(0)
    }
  })

  it("heals after a job that failed, the job's failure kept", async () => {
    const { root, rig } = await samples()
    const { sink } = collecting()
    const exit = await exitOf(Setup.around(plan(rig, root, ["first", "second"]), sink, Effect.fail("the job failed")))
    expect(exit).toEqual(Exit.fail("the job failed"))
    expect(traceOf(root)).toEqual(["inject first", "inject second", "heal second", "heal first"])
  })

  it("heals after a job that was interrupted, the interruption kept", async () => {
    const { root, rig } = await samples()
    const { sink, phases } = collecting()
    const exit = await Effect.runPromise(Effect.gen(function*() {
      const fiber = yield* Effect.forkChild(Setup.around(plan(rig, root, ["first"]), sink, Effect.never))
      yield* Effect.sleep("20 millis").pipe(Effect.repeat({ until: () => phases().includes("injected first") }))
      return yield* Fiber.interrupt(fiber).pipe(Effect.andThen(Fiber.await(fiber)))
    }))
    expect(Exit.hasInterrupts(exit)).toBe(true)
    expect(traceOf(root)).toEqual(["inject first", "heal first"])
    expect(phases().slice(-3)).toEqual(["heal first", "healed first", "recovered first"])
  })

  it("on a failed inject runs no job and heals that operation and those before it, in reverse", async () => {
    const { root, rig } = await samples()
    const { sink, phases } = collecting()
    const exit = await exitOf(
      Setup.around(plan(rig, root, ["first", "failing-inject", "second"]), sink, Effect.sync(() => "the job ran"))
    )
    expect(Exit.isFailure(exit) && String(exit.cause)).toContain("failing-inject: inject exited 3")
    expect(traceOf(root)).toEqual(["inject first", "inject failing-inject", "heal failing-inject", "heal first"])
    expect(phases()).toContain("failed failing-inject")
  })

  it("kills an inject that overran its timeout, its whole process group, and heals it", async () => {
    const { root, rig } = await samples()
    const { events, sink } = collecting()
    const planned = plan(rig, root, ["hanging-inject"])
    const exit = await exitOf(Setup.around(planned, sink, Effect.void))
    expect(Exit.isFailure(exit) && String(exit.cause)).toContain("timed out after 500 ms")
    const sleeper = Number(fs.readFileSync(path.join(planned[0]!.dir, "state", "sleeper"), "utf8"))
    expect(() => process.kill(sleeper, 0)).toThrow()
    expect(events.find((event) => event.phase === "failed")?.message).toMatch(/^inject timed out after 500 ms, its log .*inject\.log$/)
    expect(traceOf(root)).toEqual(["inject hanging-inject", "heal hanging-inject"])
  })

  it("gives inject and heal one state directory, and the context of the job and the lane", async () => {
    const { root, rig } = await samples()
    const { sink } = collecting()
    const store = "targets/records-store/lab-1.toml"
    await exitOf(Setup.around(plan(rig, root, ["stateful"], [laneOf("bench", { "records-store": store })]), sink, Effect.void))
    expect(traceOf(root)).toEqual(["inject stateful", "heal stateful token"])
    const context = JSON.parse(fs.readFileSync(path.join(root, "context-seen.json"), "utf8"))
    expect(context).toMatchObject({ schema: "context.v1", scope: "job", cells: [], params: { grace_s: 5 } })
    expect(context.lane).toEqual({
      name: "bench",
      clock: "simulated",
      sip: {},
      backend: {},
      parts: { "records-store": { name: "lab-1", file: store, value: { file: store } } }
    })
  })

  it("marks the lab left changed when a heal fails twice, having run it twice", async () => {
    const { root, rig } = await samples()
    const { events, sink, phases } = collecting()
    const exit = await exitOf(Setup.around(plan(rig, root, ["first", "failing-heal"]), sink, Effect.succeed("done")))
    expect(exit).toEqual(Exit.succeed({ value: "done", labLeftChanged: true }))
    expect(traceOf(root)).toEqual(["inject first", "inject failing-heal", "heal failing-heal", "heal failing-heal", "heal first"])
    expect(phases().filter((phase) => phase.endsWith("failing-heal")).slice(2)).toEqual([
      "heal failing-heal",
      "failed failing-heal",
      "heal failing-heal",
      "failed failing-heal",
      "lab-left-changed failing-heal"
    ])
    expect(events.at(-4)?.message).toMatch(/^2 heal attempts failed, the last: heal exited 1, its log /)
  })

  it("does not mark the lab when the second heal attempt recovers", async () => {
    const { root, rig } = await samples()
    const { sink, phases } = collecting()
    const exit = await exitOf(Setup.around(plan(rig, root, ["flaky-heal"]), sink, Effect.void))
    expect(exit).toEqual(Exit.succeed({ value: undefined, labLeftChanged: false }))
    expect(phases().slice(2)).toEqual([
      "heal flaky-heal",
      "failed flaky-heal",
      "heal flaky-heal",
      "healed flaky-heal",
      "recovered flaky-heal"
    ])
  })

  it("calls a probe answering 75 again until it answers 0", async () => {
    const { root, rig } = await samples()
    const { sink, phases } = collecting()
    const exit = await exitOf(Setup.around(plan(rig, root, ["late-recovery"]), sink, Effect.void))
    expect(exit).toEqual(Exit.succeed({ value: undefined, labLeftChanged: false }))
    expect(traceOf(root).filter((line) => line.startsWith("probe"))).toHaveLength(3)
    expect(phases().slice(-1)).toEqual(["recovered late-recovery"])
  })

  it("fails a heal attempt whose probe still answers 75 at its bound, and marks the lab after two", async () => {
    const { root, rig } = await samples()
    const { events, sink } = collecting()
    const exit = await exitOf(Setup.around(plan(rig, root, ["never-recovers"]), sink, Effect.void))
    expect(exit).toEqual(Exit.succeed({ value: undefined, labLeftChanged: true }))
    expect(traceOf(root).filter((line) => line === "heal never-recovers")).toHaveLength(2)
    expect(events.filter((event) => event.phase === "failed").map((event) => event.message?.split(",")[0])).toEqual([
      "probe still answers not yet after 0.3 s",
      "probe still answers not yet after 0.3 s"
    ])
  })
})

describe("a setup's plan", () => {
  it("refuses a name no operation of the path defines, naming the file stating it", async () => {
    const { rig } = await samples()
    const planned = Setup.planOf(rig, "rig.toml", ["first", "restart-everything"], [], "/job")
    expect(Result.isFailure(planned) && planned.failure.message).toBe(
      "rig.toml: setup: names restart-everything, which no operation of the path defines"
    )
  })

  it("refuses an operation whose parts no lane of the job names", async () => {
    const { rig } = await samples()
    const planned = Setup.planOf(rig, "rig.toml", ["needs-records"], [laneOf("bench")], "/job")
    expect(Result.isFailure(planned) && planned.failure.reason).toBe(
      "setup: needs-records needs the parts records-store, which no lane of the job names all of"
    )
  })

  it("runs an operation that needs parts once per distinct part file, the first lane naming it lending its context", async () => {
    const { rig } = await samples()
    const lanes = [
      laneOf("a", { "records-store": "targets/records-store/lab-1.toml" }),
      laneOf("b", { "records-store": "targets/records-store/lab-1.toml" }),
      laneOf("c", { "records-store": "targets/records-store/lab-2.toml" }),
      laneOf("d")
    ]
    const planned = Setup.planOf(rig, "rig.toml", ["first", "needs-records"], lanes, "/job")
    expect(Result.isSuccess(planned) && planned.success.map((one) => [one.label, one.lane?.id, one.dir])).toEqual([
      ["first", undefined, "/job/1-first"],
      ["needs-records for a", "a", "/job/2-needs-records-a"],
      ["needs-records for c", "c", "/job/2-needs-records-c"]
    ])
  })
})
