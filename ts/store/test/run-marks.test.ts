/**
 * What a run row records after its cells landed, against a real postgres: the
 * post-run cleanup gate's finding on the batches its cells ran in, and the
 * close of a sweep whose process is gone. The suite builds and drops its own
 * database.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import { CURRENT } from "./identity.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_run_marks_test")

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

const withStore = <A, E>(effect: Effect.Effect<A, E, Runs.Service | SqlClient>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Runs.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const served = Server.served

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
  await on("postgres", Server.dropDatabase(SCRATCH))
}, 60_000)

const open = (sweepId?: number) =>
  Effect.flatMap(Runs.Service, (runs) =>
    runs.open({
      capture: "marks.pcap",
      lane: "real",
      mode: "record",
      sessionDir: "/w/marks",
      identity: CURRENT,
      ...(sweepId === undefined ? {} : { sweepId })
    })
  )

const sweep = Effect.gen(function*() {
  const sql = yield* SqlClient
  const rows = yield* sql<{ id: string }>`
    INSERT INTO sweeps (lane, universe, captures, concurrency, batch_size, memory_max)
    VALUES ('real', 'named', 2, 1, 1, '1G') RETURNING id
  `
  return Number(rows[0]!.id)
})

describe.skipIf(!served)("run marks", () => {
  it("records a dirty cleanup gate on every run a batch touched, each finding kept", async () => {
    const [a, b] = await withStore(Effect.all([open(), open()]))
    await withStore(Effect.flatMap(Runs.Service, (runs) => runs.dirty([a!.id, b!.id], "1 call left")))
    await withStore(Effect.flatMap(Runs.Service, (runs) => runs.dirty([b!.id], "2 calls left")))
    const rows = await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      return yield* sql<{ id: string; cleanup: string | null }>`
        SELECT id, cleanup FROM runs WHERE id IN ${sql.in([a!.id, b!.id])} ORDER BY id
      `
    }))
    expect(rows.map((row) => row.cleanup)).toEqual(["1 call left", "1 call left; 2 calls left"])
  })

  it("names the open runs of a sweep with their bundles, and none of another sweep", async () => {
    const dead = await withStore(sweep)
    const alive = await withStore(sweep)
    const [first, second] = await withStore(Effect.all([open(dead), open(dead)]))
    await withStore(open(alive))
    await withStore(Effect.flatMap(Runs.Service, (runs) => runs.finish({ id: first!.id, status: "done", cases: [] })))
    const left = await withStore(Effect.flatMap(Runs.Service, (runs) => runs.openOfSweep(dead)))
    expect(left).toEqual([{ id: second!.id, bundlePath: second!.bundlePath }])
  })
})
