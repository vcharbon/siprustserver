/**
 * A run whose row is gone, read off the summary a citation kept: the run's
 * own line, every cell it held with its verdict, step and main error, and the
 * records citing it. A run no summary kept answers nothing. The suite builds
 * and drops its OWN database and skips where no server answers.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as RunSummaries from "../src/run-summaries.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_run_summaries_test")

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

const read = (runId: number) =>
  Effect.runPromise(
    Effect.flatMap(RunSummaries.Service, (summaries) => summaries.of(runId)).pipe(
      Effect.provide(RunSummaries.layer),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const CODE = JSON.stringify([{ name: "app", sha: "a".repeat(40), dirty: false, tree: "t1" }])

const seed = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql.unsafe(`
    INSERT INTO run_summaries (run_id, capture, lane, set_kind, status, detail, cases, failed, env, branch,
                               components, sweep_id, job_id, prep_id, started_at, finished_at)
    VALUES (41, 'trace.pcap', 'fake', 'full', 'failed', NULL, 2, 1, 'main-checkout', 'main',
            '${CODE}'::jsonb, 7, NULL, 3, '2026-10-01T10:00:00Z', '2026-10-01T10:01:00Z')
  `)
  yield* sql.unsafe(`
    INSERT INTO cell_summaries (run_id, case_id, capture, lane, call_key, passed, verdict, skipped, failed_step,
                                main_error, components, env, branch, sweep_id, job_id, prep_id, started_at)
    VALUES
      (41, 'trace.pcap-case2', 'trace.pcap', 'fake', 'k2', true, 'ok', NULL, NULL, NULL,
       '${CODE}'::jsonb, 'main-checkout', 'main', 7, NULL, 3, '2026-10-01T10:00:00Z'),
      (41, 'trace.pcap-case1', 'trace.pcap', 'fake', 'k1', false, 'failed', NULL, 's6',
       '{"kind":"died","rule":"expect-timed-out","rfc":null,"step":"s6","expected":"200 to INVITE","detail":""}'::jsonb,
       '${CODE}'::jsonb, 'main-checkout', 'main', 7, NULL, 3, '2026-10-01T10:00:00Z')
  `)
  yield* sql.unsafe(`
    INSERT INTO citations (kind, ref, cited_by, cited_on)
    VALUES ('run', '41', 'tracker/12', '2026-10-02'), ('run', '41', 'tracker/3', NULL)
  `)
})

beforeAll(async () => {
  if (!Server.served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* Server.dropDatabase(SCRATCH)
      yield* sql.unsafe(`CREATE DATABASE ${SCRATCH}`)
    })
  )
  await on(SCRATCH, Server.migrated)
  await on(SCRATCH, seed)
}, 60_000)

afterAll(async () => {
  if (!Server.served) return
  await on("postgres", Server.dropDatabase(SCRATCH))
}, 60_000)

describe.skipIf(!Server.served)("RunSummaries.of", () => {
  it("reads a summarised run: its line, its cells in case order, and who cites it", async () => {
    const kept = await read(41)
    expect(kept).not.toBeNull()
    expect(kept!.run).toMatchObject({
      runId: 41,
      capture: "trace.pcap",
      lane: "fake",
      setKind: "full",
      status: "failed",
      cases: 2,
      failed: 1,
      env: "main-checkout",
      branch: "main",
      sweepId: 7,
      jobId: null,
      prepId: 3,
      startedAt: "2026-10-01T10:00:00.000Z",
      finishedAt: "2026-10-01T10:01:00.000Z",
      citedBy: ["tracker/12", "tracker/3"]
    })
    expect(kept!.run.code).toEqual([{ name: "app", sha: "a".repeat(40), dirty: false, tree: "t1" }])
    expect(kept!.cells.map((cell) => [cell.caseId, cell.passed, cell.verdict, cell.failedStep, cell.callKey])).toEqual([
      ["trace.pcap-case1", false, "failed", "s6", "k1"],
      ["trace.pcap-case2", true, "ok", null, "k2"]
    ])
    expect(kept!.cells[0]!.mainError).toEqual({
      kind: "died",
      rule: "expect-timed-out",
      rfc: null,
      step: "s6",
      expected: "200 to INVITE",
      detail: ""
    })
  })

  it("answers null for a run no summary kept", async () => {
    expect(await read(42)).toBeNull()
  })
})
