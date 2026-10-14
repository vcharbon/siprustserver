/**
 * The source-fault migration against a real postgres: a store migrated up to
 * the migration before it, holding rows whose main error is the class under
 * its former token, keeps each fault as the row's source fault and its main
 * error unmeasured (a source fault is no main error), and files the rule counts
 * under `source-fault`. The suite builds and drops its own database.
 */
import * as Effect from "effect/Effect"
import * as Migrator from "effect/unstable/sql/Migrator"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Migrations from "../src/migrations.js"
import * as StoreMigrator from "../src/migrator.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_source_fault_test")

const served = Server.served

/** Every migration before the source fault's, alone. */
const before = Migrator.make({})({
  loader: Migrator.fromRecord(
    Object.fromEntries(Object.entries(Migrations.record).filter(([key]) => key !== Migrations.SOURCE_FAULT))
  ),
  table: Migrations.TABLE
})

beforeAll(async () => {
  if (!served) return
  await Server.fresh(SCRATCH)
}, 60_000)

afterAll(async () => {
  if (!served) return
  await Server.drop(SCRATCH)
}, 60_000)

describe.skipIf(!served)("the source-fault migration", () => {
  it("keeps each stored row's fault of the former token as its source fault, its main error unmeasured", () =>
    Server.on(
      SCRATCH,
      Effect.gen(function*() {
        const sql = yield* SqlClient
        yield* before
        const [run] = yield* sql<{ id: number }>`
          INSERT INTO runs (capture, lane, mode, status, bundle_path)
          VALUES ('capture_fake.pcap.gz', 'fake', 'record', 'done', 'capture_fake.pcap.gz/run1')
          RETURNING id
        `
        yield* sql`
          INSERT INTO case_results (run_id, case_id, passed, verdict, error_kind, error_rule, error_detail)
          VALUES (${run!.id}, 'capture_fake.pcap.gz-case1', false, 'ok', 'harness-fault', 'no-200-after-cancel',
                  'uas1: answered 200 after the CANCEL')
        `
        yield* sql`
          INSERT INTO cell_summaries (run_id, case_id, capture, lane, passed, verdict, main_error, started_at)
          VALUES (${run!.id}, 'capture_fake.pcap.gz-case1', 'capture_fake.pcap.gz', 'fake', false, 'ok',
                  ${JSON.stringify({ kind: "harness-fault", rule: "no-200-after-cancel" })}::jsonb, now())
        `
        yield* sql`
          INSERT INTO run_rule_hits (run_id, family, rule, bucket, hits, cells)
          VALUES (${run!.id}, 'rfc', 'no-200-after-cancel', 'harness-fault', 1, 1)
        `

        yield* StoreMigrator.run

        const source = { rule: "no-200-after-cancel", rfc: null, detail: "uas1: answered 200 after the CANCEL" }
        const results = yield* sql<{ error_kind: string | null; error_rule: string | null; error_source: unknown }>`
          SELECT error_kind, error_rule, error_source FROM case_results
        `
        expect(results).toEqual([{ error_kind: null, error_rule: null, error_source: source }])
        const summaries = yield* sql<{ main_error: unknown }>`SELECT main_error FROM cell_summaries`
        expect(summaries).toEqual([{ main_error: null }])
        const hits = yield* sql<{ bucket: string }>`SELECT bucket FROM run_rule_hits`
        expect(hits).toEqual([{ bucket: "source-fault" }])
        const viewed = yield* sql<{ error_source: unknown }>`SELECT error_source FROM run_cells`
        expect(viewed).toEqual([{ error_source: source }])
      })
    ), 60_000)
})
