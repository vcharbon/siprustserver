/**
 * The cross-lane delta, against a real postgres.
 *
 * Every claim here is about which cells the join keeps and which it drops, and
 * the two that matter most are the drops: a cell the reference lane SKIPPED has
 * no green to be red against and must not be counted as a delta, and it must
 * not vanish either — it is the one place a strict-lane failure can hide.
 *
 * The suite builds and drops its OWN database, so the server's other databases are
 * never touched (`./server.js`).
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Delta from "../src/delta.js"
import type { CaseResult } from "../src/model.js"
import * as Runs from "../src/runs.js"
import { CURRENT } from "./identity.js"
import * as Sessions from "../src/sessions.js"

const SCRATCH = await Server.scratch("store_delta_test")
const REAL = "local-real"
const FAKE = "fake"

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withDelta = <A, E>(
  effect: Effect.Effect<A, E, Delta.Service | Runs.Service | SqlClient>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Delta.layer),
      Effect.provide(Runs.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

const green = (caseId: string): CaseResult => ({
  caseId,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null
})

const red = (caseId: string, failedStep: string | null): CaseResult => ({
  ...green(caseId),
  passed: false,
  verdict: "failed",
  failedStep
})

const blocked = (caseId: string, reason: string): CaseResult => ({
  ...green(caseId),
  passed: false,
  verdict: "absent",
  skipped: `blocked:${reason}`
})

/** One finished run of `capture` on `lane`; answers its id. */
const ran = Effect.fn("delta-test.ran")(function*(
  capture: string,
  lane: string,
  cases: ReadonlyArray<CaseResult>
) {
  const runs = yield* Runs.Service
  const run = yield* runs.open({ capture, lane, mode: "record", sessionDir: "/w/s", identity: CURRENT })
  yield* runs.finish({ id: run.id, status: "done", cases })
  return run.id
})

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

describe.skipIf(!served)("Delta", () => {
  it("lists the cells green on the reference lane and red on this one", () =>
    withDelta(Effect.gen(function*() {
      const capture = "capture_delta.pcap.gz"
      yield* ran(capture, FAKE, [green(`${capture}-case1`), green(`${capture}-case2`)])
      yield* ran(capture, REAL, [green(`${capture}-case1`), red(`${capture}-case2`, "s7")])

      const rows = yield* (yield* Delta.Service).rows(REAL, FAKE)
      expect(rows.map((row) => [row.caseId, row.failedStep])).toEqual([
        [`${capture}-case2`, "s7"]
      ])
      const population = yield* (yield* Delta.Service).population(REAL, FAKE)
      expect(population.compared).toEqual(2)
      expect(population.green).toEqual(2)
      expect(population.unasked).toEqual(0)
    })))

  it("reads only each lane's LATEST run, so a fixed cell leaves the list", () =>
    withDelta(Effect.gen(function*() {
      const capture = "capture_latest.pcap.gz"
      yield* ran(capture, FAKE, [green(`${capture}-auto`)])
      yield* ran(capture, REAL, [red(`${capture}-auto`, "s3")])
      const before = yield* (yield* Delta.Service).rows(REAL, FAKE)
      expect(before.map((row) => row.caseId)).toContain(`${capture}-auto`)

      yield* ran(capture, REAL, [green(`${capture}-auto`)])
      const after = yield* (yield* Delta.Service).rows(REAL, FAKE)
      expect(after.map((row) => row.caseId)).not.toContain(`${capture}-auto`)
    })))

  it("keeps a red-on-both cell out of the list: it was never green anywhere", () =>
    withDelta(Effect.gen(function*() {
      const capture = "capture_bothred.pcap.gz"
      yield* ran(capture, FAKE, [red(`${capture}-auto`, "s2")])
      yield* ran(capture, REAL, [red(`${capture}-auto`, "s2")])

      const delta = yield* Delta.Service
      expect((yield* delta.rows(REAL, FAKE)).map((row) => row.caseId))
        .not.toContain(`${capture}-auto`)
      expect((yield* delta.unasked(REAL, FAKE)).map((row) => row.caseId))
        .not.toContain(`${capture}-auto`)
    })))

  it("answers a cell the reference lane BLOCKED separately, never as a delta", () =>
    withDelta(Effect.gen(function*() {
      const capture = "capture_blocked.pcap.gz"
      yield* ran(capture, FAKE, [blocked(`${capture}-case2`, "multi-party-not-driveable")])
      yield* ran(capture, REAL, [red(`${capture}-case2`, "s7")])

      const delta = yield* Delta.Service
      // Not a delta: nothing says this cell ever passed anywhere.
      expect((yield* delta.rows(REAL, FAKE)).map((row) => row.caseId))
        .not.toContain(`${capture}-case2`)
      // But not dropped either — this is the only place it can be seen.
      expect((yield* delta.unasked(REAL, FAKE)).map((row) => row.caseId))
        .toContain(`${capture}-case2`)
      const population = yield* delta.population(REAL, FAKE)
      expect(population.unasked).toBeGreaterThan(0)
    })))

  it("ignores a cell THIS lane skipped: the lane was never asked either", () =>
    withDelta(Effect.gen(function*() {
      const capture = "capture_selfblocked.pcap.gz"
      yield* ran(capture, FAKE, [green(`${capture}-auto`)])
      yield* ran(capture, REAL, [blocked(`${capture}-auto`, "no-sut")])

      const delta = yield* Delta.Service
      expect((yield* delta.rows(REAL, FAKE)).map((row) => row.caseId))
        .not.toContain(`${capture}-auto`)
      expect((yield* delta.unasked(REAL, FAKE)).map((row) => row.caseId))
        .not.toContain(`${capture}-auto`)
    })))

  it("states the population and the shape above the list", () => {
    const population: Delta.Population = {
      lane: REAL,
      against: FAKE,
      compared: 1000,
      green: 1000,
      sweeps: [3, 5, 8],
      unasked: 17
    }
    const rows: ReadonlyArray<Delta.Row> = [
      { capture: "c.pcap.gz", caseId: "c.pcap.gz-case2", failedStep: "s7", verdict: "failed", runId: 1, sweepId: 8 },
      { capture: "d.pcap.gz", caseId: "d.pcap.gz-auto", failedStep: null, verdict: "failed", runId: 2, sweepId: 8 }
    ]
    const out = Delta.render(population, rows, [rows[0]!], 10)
    expect(out).toContain("2 cell(s)")
    expect(out).toContain("over 1000 cell(s) both lanes ran")
    expect(out).toContain("sweeps 3..8 (3)")
    expect(out).toContain("by step: -×1  s7×1")
    expect(out).toContain("BESIDE IT: 1 cell(s)")
  })

  it("says there is nothing to work rather than printing an empty list", () => {
    const out = Delta.render(
      { lane: REAL, against: FAKE, compared: 40, green: 40, sweeps: [7], unasked: 0 },
      [],
      [],
      10
    )
    expect(out).toContain("0 cell(s)")
    expect(out).toContain("nothing to work")
    expect(out).toContain("sweep 7")
  })
})
