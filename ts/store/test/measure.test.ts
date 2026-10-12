/**
 * The measurement side of the per-call record, against a real postgres: which
 * runs the queue reads still carry failing rows nobody measured, and the
 * re-measure that fills a run's error columns and touches nothing else it says.
 * The suite builds and drops its OWN database and skips where no server answers.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Measure from "../src/measure.js"
import type { CaseResult, MainError } from "../src/model.js"
import * as Runs from "../src/runs.js"
import { CURRENT } from "./identity.js"
import * as Sessions from "../src/sessions.js"

const SCRATCH = await Server.scratch("store_measure_test")
const LANE = "fake"

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withStore = <A, E>(
  effect: Effect.Effect<A, E, Measure.Service | Runs.Service | SqlClient>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Measure.layer),
      Effect.provide(Runs.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

const DIED: MainError = {
  kind: "died",
  rule: null,
  rfc: null,
  step: "s14",
  expected: "200 to INVITE",
  detail: "gated on early dialog \"f5\""
}

const caseOf = (caseId: string, rest: Partial<CaseResult>): CaseResult => ({
  caseId,
  passed: false,
  verdict: "failed",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null,
  ...rest
})

const run = (capture: string, cases: ReadonlyArray<CaseResult>, setKind: "full" | "cases" = "full") =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const opened = yield* runs.open({
      capture,
      lane: LANE,
      mode: "record",
      sessionDir: `/w/${capture}`,
      setKind,
      identity: CURRENT
    })
    yield* runs.finish({ id: opened.id, status: "done", cases })
    return opened
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

describe.skipIf(!served)("Measure", () => {
  it("re-measures a run's error columns without touching its verdict", () =>
    withStore(Effect.gen(function*() {
      const measure = yield* Measure.Service
      const sql = yield* SqlClient
      const capture = "capture_remeasured.pcap.gz"
      const opened = yield* run(capture, [
        caseOf(`${capture}-case1`, { failedStep: "s14" }),
        caseOf(`${capture}-case2`, { passed: true, verdict: "ok" })
      ])

      // Recorded before the columns existed: a failing row with no measure.
      const unmeasured = yield* measure.unmeasured(LANE)
      expect(unmeasured.map((one) => one.id)).toContain(opened.id)

      const updated = yield* measure.remeasure(opened.id, [
        caseOf(`${capture}-case1`, { passed: true, verdict: "ok", mainError: DIED }),
        caseOf(`${capture}-case2`, { passed: true, verdict: "ok" })
      ])
      expect(updated).toEqual(2)
      const rows = yield* sql<{ case_id: string; passed: boolean; error_kind: string | null; error_step: string | null }>`
        SELECT case_id, passed, error_kind, error_step FROM case_results WHERE run_id = ${opened.id} ORDER BY case_id
      `
      expect(rows).toEqual([
        { case_id: `${capture}-case1`, passed: false, error_kind: "died", error_step: "s14" },
        { case_id: `${capture}-case2`, passed: true, error_kind: null, error_step: null }
      ])
      expect((yield* measure.unmeasured(LANE)).map((one) => one.id)).not.toContain(opened.id)
    })))

  it("offers the runs the queue reads: the standing full run and a newer cases run, not an older one", () =>
    withStore(Effect.gen(function*() {
      const measure = yield* Measure.Service
      const capture = "capture_reads.pcap.gz"
      const stale = yield* run(capture, [caseOf(`${capture}-case1`, {})])
      const full = yield* run(capture, [caseOf(`${capture}-case1`, {}), caseOf(`${capture}-case2`, {})])
      const rerun = yield* run(capture, [caseOf(`${capture}-case1`, {})], "cases")
      const offered = (yield* measure.unmeasured(LANE)).map((one) => one.id)
      expect(offered).toContain(full.id)
      expect(offered).toContain(rerun.id)
      expect(offered).not.toContain(stale.id)
    })))
})
