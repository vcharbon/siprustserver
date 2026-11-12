/**
 * Which campaign job row a run answers for: the one it was launched as,
 * closed even when the run lock is refused; else its own, made only once the
 * lock holds; else, for a quick run, none; a run stopped once its setup left
 * the lab changed closes its row failed. The suite builds and drops its OWN
 * database (`@sip/store/test-server`).
 */
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import { JobEvents, Jobs } from "@sip/store"
import * as Deferred from "effect/Deferred"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as fs from "node:fs"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { RunHeld } from "../src/exclusive.js"
import { asJob, type Launch } from "../src/job.js"
import { setupOf, sinkOf } from "../src/setup-phase.js"
import { planned, traceOf, workspace } from "./setup-operations.js"

const SCRATCH = await scratch("campaign_runner_job_test")

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

const withStore = <A, E>(effect: Effect.Effect<A, E, Jobs.Service | JobEvents.Service | SqlClient>) =>
  Effect.runPromise(
    effect.pipe(Effect.provide(Jobs.layer), Effect.provide(JobEvents.layer), Effect.provide(layerFromUrl(urlFor(SCRATCH))))
  )

const launch: Launch = { capture: "basic-call", lanes: ["sim"], case: null, subcase: null, rebuild: false, allow: [] }

/** The campaign rows `createdBy` made, with how each stands. */
const rowsBy = (createdBy: string) =>
  Effect.flatMap(SqlClient, (sql) =>
    sql<{ id: string; status: string; exit: number | null }>`
      SELECT id, status, exit FROM jobs WHERE created_by = ${createdBy} AND kind = 'campaign' ORDER BY id`)

/** How many campaign rows the store holds. */
const campaignRows = Effect.flatMap(SqlClient, (sql) =>
  Effect.map(sql<{ n: string }>`SELECT count(*) AS n FROM jobs WHERE kind = 'campaign'`, (rows) => Number(rows[0]!.n)))

const VERB = "campaign run"

const refused = Effect.fail(new RunHeld({ verb: VERB, env: "dev" }))

describe.skipIf(!served)("the job a run answers for", () => {
  it("makes no row of its own for a run whose lock is refused", async () => {
    const { exit, rows } = await withStore(Effect.gen(function*() {
      const exit = yield* Effect.exit(asJob(VERB, undefined, { launch, createdBy: "held" }, refused, () => Effect.succeed(0 as const)))
      return { exit, rows: yield* rowsBy("held") }
    }))
    expect(Exit.isFailure(exit)).toBe(true)
    expect(rows).toEqual([])
  })

  it("makes its own row once the lock holds, the body runs under it, and closes it as the run ended", async () => {
    const { seen, rows } = await withStore(Effect.gen(function*() {
      let seen: number | undefined
      yield* asJob(VERB, undefined, { launch, createdBy: "own" }, Effect.void, (job) => Effect.sync(() => {
        seen = job
        return 1 as const
      }))
      return { seen, rows: yield* rowsBy("own") }
    }))
    expect(rows.map((row) => [Number(row.id), row.status, row.exit])).toEqual([[seen, "done", 1]])
  })

  it("closes its row only once the run lock is released", async () => {
    const { atRelease, rows } = await withStore(Effect.gen(function*() {
      let atRelease: ReadonlyArray<string> = []
      const hold = Effect.acquireRelease(Effect.void, () =>
        Effect.orDie(Effect.map(rowsBy("released"), (found) => void (atRelease = found.map((row) => row.status)))))
      yield* asJob(VERB, undefined, { launch, createdBy: "released" }, hold, () => Effect.succeed(0 as const))
      return { atRelease, rows: yield* rowsBy("released") }
    }))
    expect(atRelease).toEqual(["running"])
    expect(rows.map((row) => row.status)).toEqual(["done"])
  })

  it("closes the row it was launched as, failed, when the lock is refused", async () => {
    const { rows } = await withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const launched = yield* jobs.create({ kind: "campaign", args: { ...launch }, createdBy: "launched" })
      yield* Effect.exit(asJob(VERB, launched, { launch, createdBy: "launched" }, refused, () => Effect.succeed(0 as const)))
      return { rows: yield* rowsBy("launched") }
    }))
    expect(rows.map((row) => [row.status, row.exit])).toEqual([["failed", 2]])
  })

  it("runs a launched run under the row it was launched as, making none of its own, and closes it as the run ended", async () => {
    const { launched, seen, rows } = await withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const launched = yield* jobs.create({ kind: "campaign", args: { ...launch }, createdBy: "relaunched" })
      let seen: number | undefined
      yield* asJob(VERB, launched, { launch, createdBy: "relaunched" }, Effect.void, (job) => Effect.sync(() => {
        seen = job
        return 0 as const
      }))
      return { launched, seen, rows: yield* rowsBy("relaunched") }
    }))
    expect(seen).toBe(launched)
    expect(rows.map((row) => [Number(row.id), row.status, row.exit])).toEqual([[launched, "done", 0]])
  })

  it("runs a quick run as no job, making no row", async () => {
    const { seen, before, after } = await withStore(Effect.gen(function*() {
      const before = yield* campaignRows
      let seen: number | undefined = -1
      yield* asJob(VERB, undefined, undefined, Effect.void, (job) => Effect.sync(() => {
        seen = job
        return 0 as const
      }))
      return { seen, before, after: yield* campaignRows }
    }))
    expect(seen).toBeUndefined()
    expect(after).toBe(before)
  })

  it("closes failed a run stopped once its setup left the lab changed", async () => {
    const root = workspace()
    try {
      const instances = await planned(root, ["stuck"])
      const { rows, marked } = await withStore(Effect.gen(function*() {
        const events = yield* JobEvents.Service
        const injected = yield* Deferred.make<void>()
        let id: number | undefined
        const run = asJob(VERB, undefined, { launch, createdBy: "stopped-marked" }, Effect.void, (job) => {
          id = job
          const sink = sinkOf({ id: job!, events })
          const told = (event: Parameters<typeof sink>[0]) =>
            Effect.andThen(sink(event), event.phase === "injected" ? Deferred.succeed(injected, undefined) : Effect.void)
          return Effect.as(setupOf(instances, told, () => Effect.void)(Effect.never), 0 as const)
        })
        const fiber = yield* Effect.forkChild(run)
        yield* Deferred.await(injected)
        yield* Fiber.interrupt(fiber)
        return { rows: yield* rowsBy("stopped-marked"), marked: yield* events.labLeftChanged(id!) }
      }))
      expect(traceOf(root)).toEqual(["inject stuck", "heal stuck", "heal stuck"])
      expect(marked).toBe(true)
      expect(rows.map((row) => [row.status, row.exit])).toEqual([["failed", 2]])
    } finally {
      fs.rmSync(root, { recursive: true, force: true })
    }
  })
})
