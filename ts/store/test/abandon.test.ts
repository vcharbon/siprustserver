/**
 * Run rows nobody will close, and the preps open runs pin — against a real
 * postgres, because every claim here is about what the QUERY selects.
 *
 * The suite builds and drops its OWN database, so the server's other databases are
 * never touched (`./server.js`).
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import * as Sweeps from "../src/sweeps.js"

const SCRATCH = await Server.scratch("store_abandon_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withRuns = <A, E>(
  effect: Effect.Effect<A, E, Runs.Service | Sweeps.Service | SqlClient>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Runs.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Sweeps.layer),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

const sweepOf = (lane: string) =>
  Effect.gen(function*() {
    const sweeps = yield* Sweeps.Service
    return yield* sweeps.open({
      lane,
      universe: "all",
      selection: "--all",
      captures: 1,
      concurrency: 1,
      batchSize: 1,
      memoryMax: "1G"
    })
  })

const openOf = (capture: string, sweepId?: number, prepId?: number) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    return yield* runs.open({
      capture,
      lane: "fake",
      mode: "record",
      sessionDir: "/w/s",
      ...(sweepId === undefined ? {} : { sweepId }),
      ...(prepId === undefined ? {} : { prepId })
    })
  })

/** Backdates a row, which is the one thing a test cannot do by running slowly. */
const age = (id: number, hours: number) =>
  Effect.gen(function*() {
    const sql = yield* SqlClient
    yield* sql`
      UPDATE runs SET started_at = now() - make_interval(hours => ${hours}) WHERE id = ${id}
    `
  })

const ids = (rows: ReadonlyArray<Runs.Abandonable>): ReadonlyArray<number> => rows.map((r) => r.id)

beforeAll(async () => {
  if (!served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* Server.dropDatabase(SCRATCH)
      yield* sql.unsafe(`CREATE DATABASE ${SCRATCH}`)
    })
  )
  await on(SCRATCH, Server.migrated)
}, 60_000)

afterAll(async () => {
  if (!served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      yield* Server.dropDatabase(SCRATCH)
    })
  )
}, 60_000)

describe.skipIf(!served)("abandonable", () => {
  it("offers a row whose sweep already ended, at any age", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const sweeps = yield* Sweeps.Service
      const sweep = yield* sweepOf("fake")
      const orphan = yield* openOf("capture_orphan.pcap.gz", sweep)
      const live = yield* openOf("capture_live.pcap.gz", yield* sweepOf("ref"))
      yield* sweeps.finish(sweep)

      const offered = ids(yield* runs.abandonable(12))
      expect(offered).toContain(orphan.id)
      // Its own sweep is still running, and it is seconds old: nothing says dead.
      expect(offered).not.toContain(live.id)
    })))

  it("offers a row past the horizon with no sweep to speak for it", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const old = yield* openOf("capture_old.pcap.gz")
      const young = yield* openOf("capture_young.pcap.gz")
      yield* age(old.id, 20)

      const offered = ids(yield* runs.abandonable(12))
      expect(offered).toContain(old.id)
      expect(offered).not.toContain(young.id)
    })))

  it("never offers a row that finished", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const done = yield* openOf("capture_done.pcap.gz")
      yield* age(done.id, 40)
      yield* runs.finish({ id: done.id, status: "done", cases: [] })
      expect(ids(yield* runs.abandonable(12))).not.toContain(done.id)
    })))

  it("closes a row without touching its status, and closes it once", () =>
    withRuns(Effect.gen(function*() {
      const sql = yield* SqlClient
      const runs = yield* Runs.Service
      const dead = yield* openOf("capture_dead.pcap.gz")
      yield* age(dead.id, 20)

      expect(yield* runs.abandon([dead.id], "gone")).toEqual(1)
      const rows = yield* sql<{ status: string; detail: string; finished_at: Date }>`
        SELECT status, detail, finished_at FROM runs WHERE id = ${dead.id}
      `
      expect(rows[0]!.status).toEqual("error")
      expect(rows[0]!.detail).toEqual("gone")
      expect(rows[0]!.finished_at).not.toBeNull()
      // Idempotent: a second pass finds it closed and re-stamps nothing.
      expect(yield* runs.abandon([dead.id], "gone again")).toEqual(0)
      expect(yield* runs.abandon([], "nothing")).toEqual(0)
    })))
})

describe.skipIf(!served)("unfinished", () => {
  it("names every open run with the prep it pins, and drops one that closed", () =>
    withRuns(Effect.gen(function*() {
      const sql = yield* SqlClient
      const runs = yield* Runs.Service
      const preps = yield* sql<{ id: string }>`
        INSERT INTO preps DEFAULT VALUES RETURNING id
      `
      const prep = Number(preps[0]!.id)
      const pinned = yield* openOf("capture_pinned.pcap.gz", undefined, prep)
      // A run older than the cut store is open and pins nothing.
      const unpinned = yield* openOf("capture_unpinned.pcap.gz")

      const open = yield* runs.unfinished
      expect(open).toContainEqual({ id: pinned.id, prepId: prep })
      expect(open).toContainEqual({ id: unpinned.id, prepId: null })

      yield* runs.finish({ id: pinned.id, status: "done", cases: [] })
      expect((yield* runs.unfinished).map((row) => row.id)).not.toContain(pinned.id)
    })))
})
