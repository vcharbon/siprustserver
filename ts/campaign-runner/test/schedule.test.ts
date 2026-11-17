import { type Cell, CellCollectors, Collector, CollectorPlan, type CollectorRun, type Lanes, type RigPath } from "@sip/rig"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as Fiber from "effect/Fiber"
import * as Result from "effect/Result"
import * as TestClock from "effect/testing/TestClock"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { type Locked, schedule } from "../src/schedule.js"

interface Job {
  readonly id: string
  readonly locks: ReadonlyArray<string>
}

/** Runs every job for `ms` of (test) clock time, recording when each held what. */
const tracked = (jobs: ReadonlyArray<Job>, concurrency: number, ms = 100) => {
  const running = new Set<string>()
  const overlaps: Array<string> = []
  let peak = 0
  const run = (job: Job) =>
    Effect.gen(function*() {
      for (const other of running) {
        const shared = jobs.find((j) => j.id === other)!.locks.filter((l) => job.locks.includes(l))
        if (shared.length > 0) overlaps.push(`${job.id}+${other} on ${shared.join(",")}`)
      }
      running.add(job.id)
      peak = Math.max(peak, running.size)
      yield* Effect.sleep(ms)
      running.delete(job.id)
      return job.id
    })
  return { run, overlaps, peak: () => peak }
}

const runAll = <A>(effect: Effect.Effect<A>) =>
  Effect.runPromise(
    Effect.gen(function*() {
      const fiber = yield* Effect.forkChild(effect)
      yield* TestClock.adjust(60_000)
      return yield* Fiber.join(fiber)
    }).pipe(Effect.provide(TestClock.layer()))
  )

describe("the lock scheduler", () => {
  it("never runs two cells holding one lock at once, and runs the rest beside them", async () => {
    const jobs: ReadonlyArray<Job> = [
      { id: "a", locks: ["shared-lock"] },
      { id: "b", locks: ["shared-lock"] },
      { id: "c", locks: [] },
      { id: "d", locks: ["shared-lock", "other"] },
      { id: "e", locks: ["other"] }
    ]
    const t = tracked(jobs, 10)
    const done = await runAll(schedule(jobs, { concurrency: 10 }, t.run))
    expect(done).toEqual(["a", "b", "c", "d", "e"])
    expect(t.overlaps).toEqual([])
    expect(t.peak()).toBeGreaterThan(1)
  })

  it("keeps at most `concurrency` cells running", async () => {
    const jobs = Array.from({ length: 7 }, (_, i) => ({ id: `j${i}`, locks: [] }))
    const t = tracked(jobs, 3)
    await runAll(schedule(jobs, { concurrency: 3 }, t.run))
    expect(t.peak()).toBe(3)
  })

  it("starts cells no faster than the pace", async () => {
    const starts: Array<number> = []
    const jobs = Array.from({ length: 4 }, (_, i) => ({ id: `j${i}`, locks: [] }))
    const run = (_: Job) =>
      Effect.map(Clock.currentTimeMillis, (now) => {
        starts.push(now)
      })
    await runAll(schedule(jobs, { concurrency: 10, startIntervalMs: 100 }, run))
    const gaps = starts.slice(1).map((at, i) => at - starts[i]!)
    expect(gaps.every((gap) => gap >= 100)).toBe(true)
  })

  describe("a cell whose collector holds its locks", () => {
    /** How long each cell's call lasts on the test clock. */
    const CALL_MS = 1000

    /** How long after its cell's call the `holding` collector collects. */
    const HOLDING_DELAY_S = 10

    /**
     * A rig path under a new directory defining the one cell-scope collector
     * `holding`, delayed {@link HOLDING_DELAY_S}, holding its cell's locks, whose collect passes;
     * its `close`, where `closes`, says `close` in the root's `trace`.
     */
    const holdingRig = (closes = false): { readonly root: string; readonly rig: RigPath.RigPath } => {
      const root = fs.mkdtempSync(path.join(os.tmpdir(), "schedule-holding-"))
      const dir = path.join(root, "collectors", "holding")
      fs.mkdirSync(dir, { recursive: true })
      fs.writeFileSync(path.join(dir, "collect"), `#!/bin/sh\necho '{"verification": "holding", "answer": "pass"}'\n`, { mode: 0o755 })
      if (closes) fs.writeFileSync(path.join(dir, "close"), `#!/bin/sh\necho close >> "${path.join(root, "trace")}"\n`, { mode: 0o755 })
      const manifest = Result.getOrThrow(Collector.parse(
        "collector.toml",
        `schema = "collector.v1"\nname = "holding"\nscope = "cell"\ndelay_s = ${HOLDING_DELAY_S}\nholds_cell_locks = true\n[run]\ncollect = "./collect"\n` +
          (closes ? `close = "./close"\n` : "")
      ))
      return {
        root,
        rig: {
          pack: undefined,
          collectors: new Map([["holding", { name: "holding", file: path.join(dir, "collector.toml"), dir, level: "workspace", manifest }]]),
          judges: new Map(),
          operations: new Map(),
          parts: new Map()
        }
      }
    }

    const REAL: Lanes.Loaded = {
      id: "lane-r",
      file: "lanes/lane-r.toml",
      lane: { schema: "lane.v1", clock: "real" },
      parts: new Map(),
      operations: new Map(),
      capabilities: new Set()
    }

    /** The cell `id` of {@link REAL} as its collectors read it, its directory under `root`. */
    const cellOf = (root: string, id: string): Cell.Cell => ({
      schema: "cell.v1",
      id,
      case: `case-${id}`,
      lane: REAL.id,
      batch: "batch-1",
      attempt: 1,
      clock: "real",
      keys: { call_id: [], caller: [], callee: [], request_uri_user: [] },
      legs: [],
      timeline: { start: { lane_ms: 0, wall_ms: 0 }, end: { lane_ms: 1, wall_ms: 1 } },
      claims: {},
      records: {},
      verifications: [],
      dir: path.join(root, "cells", id),
      tags: [],
      needs: []
    })

    /** The phases that start a collector's process, and those that say it ended. */
    const STEP_STARTS: ReadonlySet<CollectorRun.Phase> = new Set(["open", "collect", "close"])
    const STEP_ENDS: ReadonlySet<CollectorRun.Phase> = new Set(["opened", "collected", "closed", "failed"])

    /** The phases that answer a collector's collect. */
    const ANSWERS: ReadonlySet<CollectorRun.Phase> = new Set(["collected", "failed"])

    /**
     * What a held-lock run of `jobs` cells reports, and whether at `now` on the
     * test clock it still owes an event no clock move brings: a first start, a
     * call's end once {@link CALL_MS} passed, a collector's answer once its
     * delay passed, a collector process's end, or, after an answer while a cell
     * waits, that cell's start. Each answer of these runs releases a waiting
     * cell or comes beside one that does.
     */
    const owedOf = (jobs: number) => {
      const running = new Set<string>()
      const answered = new Set<string>()
      const calls: Array<number> = []
      const ends: Array<number> = []
      let releasing = false
      const sink: CollectorRun.Sink = (event) =>
        Effect.sync(() => {
          if (STEP_STARTS.has(event.phase)) running.add(event.collector)
          if (STEP_ENDS.has(event.phase)) running.delete(event.collector)
          if (ANSWERS.has(event.phase) && !answered.has(event.collector)) {
            answered.add(event.collector)
            releasing = calls.length < jobs
          }
        })
      const passed = (from: ReadonlyArray<number>, ms: number, now: number) => from.filter((at) => at + ms <= now).length
      return {
        sink,
        started: (now: number) => {
          calls.push(now)
          releasing = false
        },
        ended: (now: number) => void ends.push(now),
        owes: (now: number) =>
          calls.length === 0 || running.size > 0 || releasing || ends.length < passed(calls, CALL_MS, now) ||
          answered.size < passed(ends, HOLDING_DELAY_S * 1000, now)
      }
    }

    /** A real pause, for the run's fibers and processes to move. */
    const pause = Effect.promise(() => new Promise((resolve) => setTimeout(resolve, 5)))

    /**
     * `effect` on the fake clock, moved a second at a time, and only once
     * `owes` says the run owes no event at the current instant: a box that
     * slows a process or a fiber costs real time, never test-clock time.
     */
    const stepped = <A>(effect: Effect.Effect<A>, owes: (now: number) => boolean) =>
      Effect.runPromise(
        Effect.gen(function*() {
          const fiber = yield* Effect.forkChild(effect)
          for (let second = 0; second < 120 && fiber.pollUnsafe() === undefined; second++) {
            while (fiber.pollUnsafe() === undefined && owes(yield* Clock.currentTimeMillis)) yield* pause
            yield* TestClock.adjust(1000)
          }
          return yield* Fiber.join(fiber)
        }).pipe(Effect.provide(TestClock.layer()))
      )

    /**
     * `jobs` scheduled at `concurrency`, each one's call of 1 s through the
     * cell-scope collectors of {@link holdingRig}: when each started on the
     * test clock, and the rig's `trace`, where each call says `call <id>` as it
     * starts.
     */
    const heldRun = async (jobs: ReadonlyArray<Job & Pick<Locked, "alone">>, concurrency: number, closes = false) => {
      const { root, rig } = holdingRig(closes)
      const plan = CollectorPlan.cellPlanOf(rig, REAL, path.join(root, "_cells"))
      const starts = new Map<string, number>()
      const trace = path.join(root, "trace")
      const owed = owedOf(jobs.length)
      await stepped(Effect.scoped(Effect.gen(function*() {
        const cells = yield* CellCollectors.make(plan, { sink: owed.sink, locker: () => Effect.succeed(Effect.void) })
        const started = (job: Job) =>
          Effect.flatMap(Clock.currentTimeMillis, (now) =>
            Effect.sync(() => {
              starts.set(job.id, now)
              owed.started(now)
              fs.appendFileSync(trace, `call ${job.id}\n`)
            }))
        const ended = Effect.flatMap(Clock.currentTimeMillis, (now) => Effect.sync(() => owed.ended(now)))
        const call = (job: Job) => Effect.andThen(started(job), Effect.andThen(Effect.sleep(CALL_MS), ended))
        yield* schedule(
          jobs,
          { concurrency },
          (job) => cells.around(job.id, call(job), () => Effect.succeed({ cells: [cellOf(root, job.id)], unread: [], failed: [] })),
          (ran) => ran.held
        )
        yield* cells.joined
      })), owed.owes)
      const traced = fs.readFileSync(trace, "utf8").split("\n").filter((line) => line !== "")
      fs.rmSync(root, { recursive: true, force: true })
      return { starts, traced }
    }

    it("keeps a second cell naming the same lock waiting until the collector answered, the lane running a cell naming none at once", async () => {
      const { starts } = await heldRun([{ id: "a", locks: ["one"] }, { id: "b", locks: ["one"] }, { id: "c", locks: [] }], 3)
      expect(starts.get("a")).toBe(0)
      expect(starts.get("c")).toBe(0)
      expect(starts.get("b")).toBe(11_000)
    })

    it("keeps the lock until the collector closed: the second cell naming it starts after the first one's close", async () => {
      const { traced } = await heldRun([{ id: "a", locks: ["one"] }, { id: "b", locks: ["one"] }], 2, true)
      expect(traced).toEqual(["call a", "close", "call b", "close"])
    })

    it("keeps a cell running alone the lane's until its collector answered: the next one alone waits for it", async () => {
      const alone = (id: string): Job & Pick<Locked, "alone"> => ({ id, locks: [], alone: { name: id, healMs: 0 } })
      const { starts } = await heldRun([alone("x"), alone("y")], 1)
      expect(starts.get("x")).toBe(0)
      expect(starts.get("y")).toBe(11_000)
    })
  })

  describe("a cell running alone", () => {
    interface AloneJob extends Job {
      readonly alone?: { readonly name: string; readonly healMs: number }
    }

    /** Each job's start and end on the test clock, in running order. */
    const timeline = (run100: ReadonlyArray<AloneJob>, concurrency = 10) => {
      const spans: Array<{ readonly id: string; readonly from: number; readonly to: number }> = []
      const run = (job: AloneJob) =>
        Effect.gen(function*() {
          const from = yield* Clock.currentTimeMillis
          yield* Effect.sleep(100)
          spans.push({ id: job.id, from, to: yield* Clock.currentTimeMillis })
          return job.id
        })
      return { spans, done: runAll(schedule(run100, { concurrency }, run)) }
    }

    const alone = (id: string, healMs = 0): AloneJob => ({ id, locks: [], alone: { name: id, healMs } })
    const shared = (id: string): AloneJob => ({ id, locks: [] })

    it("overlaps no other cell, runs after the shared ones, and answers in the cells' order", async () => {
      const { done, spans } = timeline([alone("x"), shared("a"), alone("y"), shared("b")])
      expect(await done).toEqual(["x", "a", "y", "b"])
      const at = (id: string) => spans.find((span) => span.id === id)!
      expect(at("a").from).toBe(at("b").from)
      expect(at("x").from).toBe(at("a").to)
      expect(at("y").from).toBe(at("x").to)
    })

    it("holds the lane for its heal time before anything follows, and the last one before the batch ends", async () => {
      const { done, spans } = timeline([alone("x", 30_000), alone("y", 500)])
      const ended = done.then(() => spans)
      const [x, y] = await ended
      expect(y!.from - x!.to).toBe(30_000)
      let finishedAt = -1
      await Effect.runPromise(
        Effect.gen(function*() {
          const fiber = yield* Effect.forkChild(
            schedule([alone("last", 1_000)], { concurrency: 1 }, () => Effect.succeed("ran")).pipe(
              Effect.tap(() => Effect.map(Clock.currentTimeMillis, (now) => (finishedAt = now)))
            )
          )
          yield* TestClock.adjust(60_000)
          yield* Fiber.join(fiber)
        }).pipe(Effect.provide(TestClock.layer()))
      )
      expect(finishedAt).toBe(1_000)
    })

    it("heals after a run that failed, then fails", async () => {
      let healedAt = -1
      const out = await Effect.runPromise(
        Effect.gen(function*() {
          const fiber = yield* Effect.forkChild(
            schedule([alone("x", 2_000)], { concurrency: 1 }, () => Effect.fail("broke" as const)).pipe(
              Effect.tapError(() => Effect.map(Clock.currentTimeMillis, (now) => (healedAt = now))),
              Effect.exit
            )
          )
          yield* TestClock.adjust(60_000)
          return yield* Fiber.join(fiber)
        }).pipe(Effect.provide(TestClock.layer()))
      )
      expect(out._tag).toBe("Failure")
      expect(healedAt).toBe(2_000)
    })

    it("holds a lock another cell holds without waiting on it: the shared cells are done by then", async () => {
      const locked: AloneJob = { id: "x", locks: ["one"], alone: { name: "x", healMs: 0 } }
      const { done, spans } = timeline([locked, { id: "a", locks: ["one"] }], 1)
      expect(await done).toEqual(["x", "a"])
      expect(spans.map((span) => span.id)).toEqual(["a", "x"])
    })
  })
})
