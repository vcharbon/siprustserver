/**
 * A batch's cell-scope collectors, real executables on the runner's fake
 * clock: each cell's opened before its call and collected `delay_s` after it
 * ended while the next cells run, so a delay costs the batch once, not once
 * per cell; a cell whose collector `holds_cell_locks` keeps its locks until
 * they answered; a collector past its deadline answers `not checkable` and
 * still closes; a simulated lane runs none and says so of each wired one.
 */
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as Fiber from "effect/Fiber"
import * as Result from "effect/Result"
import * as TestClock from "effect/testing/TestClock"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as CellCollectors from "../src/cell-collectors.js"
import * as CollectorPlan from "../src/collector-plan.js"
import type * as CollectorRun from "../src/collector-run.js"
import type * as Lanes from "../src/lanes.js"
import * as RigPath from "../src/rig-path.js"
import { cellOf, memoryLocker, traceOf } from "./collectors.js"
import { laneOf } from "./operations.js"
import { type Entry, run, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const sh = (body: string): Entry => ({ text: `#!/bin/sh\n${body}\n`, executable: true })

/** A cell-scope collector `name`: its manifest (`extra` lines before `[run]`) and the steps given, each tracing itself. */
const cellCollector = (name: string, extra: string, steps: { readonly open?: string; readonly collect: string; readonly close?: string }) => {
  const named = (["open", "collect", "close"] as const).filter((step) => steps[step] !== undefined)
  return {
    [`collectors/${name}/collector.toml`]: `schema = "collector.v1"\nname = "${name}"\nscope = "cell"\nanswers = ["${name}"]\n${extra}\n[run]\n${
      named.map((step) => `${step} = "./${step}"\n`).join("")
    }`,
    ...Object.fromEntries(named.map((step) => [`collectors/${name}/${step}`, sh(`echo "${step} ${name}" >> ../../trace\n${steps[step]}`)]))
  }
}

/** The line a collect prints that names no cell: its scope's own, which is its one cell's. */
const PASS = (name: string) => `echo '{"verification": "${name}", "answer": "pass"}'`

const SAMPLES = {
  ...cellCollector("delayed", "delay_s = 10", { collect: PASS("delayed") }),
  ...cellCollector("holding", "delay_s = 10\nholds_cell_locks = true", { collect: PASS("holding") }),
  ...cellCollector("bracketed", "", { open: "exit 0", collect: PASS("bracketed"), close: "exit 0" }),
  ...cellCollector("hanging", "delay_s = 1\n[timeouts]\ncollect_s = 2", { collect: "sleep 30 &\nwait", close: "exit 0" })
}

/** A real-clock lane. */
const REAL: Lanes.Loaded = { ...laneOf("lane-a"), lane: { schema: "lane.v1", clock: "real" } }

/** The sample collectors' rig path, loaded, with only `wired` among its collectors. */
const rigWith = async (...wired: ReadonlyArray<string>) => {
  const root = tree(SAMPLES)
  made.push(root)
  const loaded = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(loaded)) throw new Error(`the samples do not load: ${String(loaded.failure)}`)
  const collectors = new Map([...loaded.success.collectors].filter(([name]) => wired.includes(name)))
  return { root, rig: { ...loaded.success, collectors } }
}

/** A sink collecting each event as `<clock ms> <phase> <collector>`. */
const collecting = () => {
  const events: Array<CollectorRun.Event> = []
  const sink: CollectorRun.Sink = (event) => Effect.sync(() => void events.push(event))
  return { events, sink, said: () => events.map((event) => `${event.clock_ms} ${event.phase} ${event.collector}`) }
}

/** The cell `id` under `root`, described. */
const overOf = (root: string, id: string): CollectorRun.Over => ({ cells: [cellOf(id, REAL.id, path.join(root, "cells", id))], unread: [], failed: [] })

/**
 * `effect` run on the fake clock, moved a second at a time with a real pause
 * between, so a collector's process ends before the clock moves on; at most
 * 400 s of it.
 */
const onFakeClock = <A, E>(effect: Effect.Effect<A, E>) =>
  Effect.runPromise(
    Effect.gen(function*() {
      const fiber = yield* Effect.forkChild(effect)
      for (let second = 0; second < 400 && fiber.pollUnsafe() === undefined; second++) {
        yield* TestClock.adjust(1000)
        yield* Effect.promise(() => new Promise((resolve) => setTimeout(resolve, 25)))
      }
      return yield* Fiber.join(fiber)
    }).pipe(Effect.provide(TestClock.layer()))
  )

/** A call that lasts `ms` on the clock. */
const call = (ms: number) => Effect.sleep(ms)

describe("a batch's cell-scope collectors", () => {
  it("delayed 10 s over 20 cells of 1 s each add about 10 s to the batch, not 200 s: the lane is not held", async () => {
    const { root, rig } = await rigWith("delayed")
    const plan = CollectorPlan.cellPlanOf(rig, REAL, path.join(root, "batch", "_cells"))
    const { events, sink } = collecting()
    const ids = Array.from({ length: 20 }, (_, n) => `c${n + 1}`)
    const { collected, endedAt } = await onFakeClock(Effect.scoped(Effect.gen(function*() {
      const cells = yield* CellCollectors.make(plan, { sink, locker: memoryLocker() })
      for (const id of ids) yield* cells.around(id, call(1000), () => Effect.succeed(overOf(root, id)))
      const collected = yield* cells.joined
      return { collected, endedAt: yield* Clock.currentTimeMillis }
    })))
    expect(endedAt).toBeGreaterThanOrEqual(30_000)
    expect(endedAt).toBeLessThan(40_000)
    expect(events.filter((event) => event.phase === "collect").map((event) => `${event.clock_ms} ${event.collector}`)).toEqual(
      ids.map((id, n) => `${(n + 1) * 1000 + 10_000} delayed of ${id}`)
    )
    expect(collected.map((one) => [...one.cells].map(([id, answers]) => `${id} ${answers.verifications.map((line) => line.answer).join(",")}`))).toEqual(
      ids.map((id) => [`${id} pass`])
    )
  })

  it("keep the locks of a cell whose collector holds_cell_locks until it answered, and give another cell's back as its call ends", async () => {
    const { root, rig } = await rigWith("holding", "delayed")
    const holding = CollectorPlan.cellPlanOf({ ...rig, collectors: new Map([...rig.collectors].filter(([name]) => name === "holding")) }, REAL, path.join(root, "a"))
    const free = CollectorPlan.cellPlanOf({ ...rig, collectors: new Map([...rig.collectors].filter(([name]) => name === "delayed")) }, REAL, path.join(root, "b"))
    const released = await onFakeClock(Effect.scoped(Effect.gen(function*() {
      const options = { sink: () => Effect.void, locker: memoryLocker() }
      const held = yield* CellCollectors.make(holding, options)
      const notHeld = yield* CellCollectors.make(free, options)
      const at = (ran: CellCollectors.Ran<void>) => Effect.andThen(ran.held, Clock.currentTimeMillis)
      const one = yield* held.around("c1", call(1000), () => Effect.succeed(overOf(root, "c1")))
      const other = yield* notHeld.around("c2", call(1000), () => Effect.succeed(overOf(root, "c2")))
      const times = yield* Effect.all([at(one), at(other)], { concurrency: "unbounded" })
      yield* held.joined
      yield* notHeld.joined
      return times
    })))
    const [heldUntil, freedAt] = released
    expect(heldUntil).toBeGreaterThanOrEqual(11_000)
    expect(heldUntil).toBeLessThan(13_000)
    expect(freedAt).toBe(2_000)
    expect(traceOf(root)).toEqual(["collect holding", "collect delayed"])
  })

  it("open before the cell's call, collect and close after it, a line naming no cell answering the cell", async () => {
    const { root, rig } = await rigWith("bracketed")
    const plan = CollectorPlan.cellPlanOf(rig, REAL, path.join(root, "_cells"))
    const collected = await onFakeClock(Effect.scoped(Effect.gen(function*() {
      const cells = yield* CellCollectors.make(plan, { sink: () => Effect.void, locker: memoryLocker() })
      const traced = Effect.sync(() => fs.appendFileSync(path.join(root, "trace"), "call c1\n"))
      yield* cells.around("c1", traced, () => Effect.succeed(overOf(root, "c1")))
      return yield* cells.joined
    })))
    expect(traceOf(root)).toEqual(["open bracketed", "call c1", "collect bracketed", "close bracketed"])
    expect(collected.map((one) => ({ scope: one.scope.verifications, c1: one.cells.get("c1")?.verifications }))).toEqual([
      { scope: [], c1: [{ name: "bracketed", source: "bracketed", answer: "pass" }] }
    ])
  })

  it("answer not checkable on the cell past their deadline of delay_s plus collect_s, and still close", async () => {
    const { root, rig } = await rigWith("hanging")
    const plan = CollectorPlan.cellPlanOf(rig, REAL, path.join(root, "_cells"))
    const { said, sink } = collecting()
    const collected = await onFakeClock(Effect.scoped(Effect.gen(function*() {
      const cells = yield* CellCollectors.make(plan, { sink, locker: memoryLocker() })
      yield* cells.around("c1", call(1000), () => Effect.succeed(overOf(root, "c1")))
      return yield* cells.joined
    })))
    const late = "not answered within its deadline of 3 s after the cell ended"
    expect(collected.map((one) => one.cells.get("c1")?.verifications)).toEqual([
      [{ name: "hanging", source: "hanging", answer: "not-checkable", message: late }]
    ])
    expect(said()).toContain(`4000 failed hanging of c1`)
    expect(traceOf(root)).toEqual(["collect hanging", "close hanging"])
  })

  it("close a cell's collectors and fail where its call fails", async () => {
    const { root, rig } = await rigWith("bracketed")
    const plan = CollectorPlan.cellPlanOf(rig, REAL, path.join(root, "_cells"))
    const failed = await onFakeClock(Effect.scoped(Effect.gen(function*() {
      const cells = yield* CellCollectors.make(plan, { sink: () => Effect.void, locker: memoryLocker() })
      return yield* Effect.flip(cells.around("c1", Effect.fail("the call could not start"), () => Effect.succeed(overOf(root, "c1"))))
    })))
    expect(failed).toBe("the call could not start")
    expect(traceOf(root)).toEqual(["open bracketed", "close bracketed"])
  })

  it("wait the lane's delay_s where it overrides the manifest's, and set the deadline by it", async () => {
    const { root, rig } = await rigWith("delayed")
    const lane: Lanes.Loaded = { ...REAL, lane: { ...REAL.lane, collectors: { delayed: { delay_s: 2 } } } }
    const instances = CollectorPlan.cellPlanOf(rig, lane, path.join(root, "_cells")).instancesFor("c1", path.join(root, "_cells", "c1"))
    expect(instances.map(CellCollectors.delayMsOf)).toEqual([2000])
    expect(CellCollectors.deadlineMsOf(instances)).toBe(302_000)
  })

  it("run none on a simulated lane, and say so of each one wired there", async () => {
    const { root, rig } = await rigWith("delayed", "holding")
    const plan = CollectorPlan.cellPlanOf(rig, laneOf("lane-s"), path.join(root, "_cells"))
    const { events, sink } = collecting()
    await Effect.runPromise(CellCollectors.notRun(plan, sink))
    expect(plan.instancesFor("c1", path.join(root, "_cells", "c1"))).toEqual([])
    expect(events.map((event) => `${event.phase} ${event.collector}: ${event.message}`)).toEqual([
      "not-run delayed: a cell-scope collector runs on no simulated lane, and lane-s is one",
      "not-run holding: a cell-scope collector runs on no simulated lane, and lane-s is one"
    ])
  })
})
