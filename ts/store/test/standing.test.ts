/**
 * What a (capture, lane) STANDS at, against a real postgres: which run is its
 * latest, which is its last green one, and which of its runs cleanup must keep.
 *
 * Every claim here is about what `standingRows` selects, so none survives a
 * fake. The rule is: only `@current` runs are read, a call stands
 * at its latest `@current` run holding it, and a `cases` run moves exactly the
 * calls it replayed.
 *
 * The suite builds and drops its OWN database, so the server's other databases are
 * never touched, and skips where no server answers.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Current from "../src/current.js"
import * as Server from "./server.js"
import { type CaseResult, isGreen } from "../src/model.js"
import type { RunIdentity } from "../src/run-identity.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import { CURRENT, CURRENT_POPULATION, TASK_ENV } from "./identity.js"

const SCRATCH = Client.scratchDatabase("store_standing_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withRuns = <A, E>(effect: Effect.Effect<A, E, Runs.Service | SqlClient>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Runs.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = await Effect.runPromise(
  Effect.gen(function*() {
    const sql = yield* SqlClient
    yield* sql`SELECT 1`
  }).pipe(
    Effect.provide(Client.layerFromUrl(urlFor("postgres"))),
    Effect.match({ onFailure: () => false, onSuccess: () => true })
  )
)

const LANE = "fake"

const passed = (caseId: string): CaseResult => ({
  caseId,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: 0,
  mainError: null,
  classification: null,
  document: null
})

const failed = (caseId: string, step = "s3"): CaseResult => ({
  ...passed(caseId),
  passed: false,
  verdict: "failed",
  failedStep: step
})

/**
 * One run of `capture` on `lane`, opened and closed on `cases`; `setKind`
 * absent is the default, `full`, and `identity` absent is `@current`.
 */
const ran = (
  capture: string,
  cases: ReadonlyArray<CaseResult>,
  setKind?: "full" | "cases",
  lane: string = LANE,
  identity: RunIdentity | null = CURRENT,
  status: "done" | "interrupted" = "done"
) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const opened = yield* runs.open({
      capture,
      lane,
      mode: "record",
      sessionDir: `/w/${capture}`,
      ...(setKind === undefined ? {} : { setKind }),
      ...(identity === null ? {} : { identity })
    })
    yield* runs.finish({ id: opened.id, status, cases })
    return opened
  })

const standingOf = (capture: string) =>
  Effect.flatMap(Runs.Service, (runs) => runs.standing(capture, LANE))

beforeAll(async () => {
  if (!served) return
  await on(
    "postgres",
    Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* sql.unsafe(`DROP DATABASE IF EXISTS ${SCRATCH} WITH (FORCE)`)
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
      const sql = yield* SqlClient
      yield* sql.unsafe(`DROP DATABASE IF EXISTS ${SCRATCH} WITH (FORCE)`)
    })
  )
}, 60_000)

describe.skipIf(!served)("a (capture, lane)'s standing", () => {
  it("is its latest run, with the latest green one as its last-ok", () =>
    withRuns(Effect.gen(function*() {
      const capture = "standing_latest.pcap.gz"
      const green = yield* ran(capture, [passed(`${capture}-auto`)])
      const red = yield* ran(capture, [failed(`${capture}-auto`)])

      const standing = yield* standingOf(capture)
      expect(standing?.runId).toBe(red.id)
      expect(standing?.failed).toBe(1)
      expect(standing?.lastOkRunId).toBe(green.id)
      expect(standing?.lastOkBundlePath).toBe(green.bundlePath)
    })))

  it("keeps the last run and the last-ok run for cleanup, and nothing older", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const capture = "standing_keep.pcap.gz"
      const older = yield* ran(capture, [passed(`${capture}-auto`)])
      const green = yield* ran(capture, [passed(`${capture}-auto`)])
      const red = yield* ran(capture, [failed(`${capture}-auto`)])

      const kept = yield* runs.protectedBundles
      expect(kept.has(red.bundlePath)).toBe(true)
      expect(kept.has(green.bundlePath)).toBe(true)
      expect(kept.has(older.bundlePath)).toBe(false)
    })))
})

describe.skipIf(!served)("only @current runs are current", () => {
  it("never moves on a task environment's run, green or red", () =>
    withRuns(Effect.gen(function*() {
      const capture = "identity_env.pcap.gz"
      const one = `${capture}-case1`
      const current = yield* ran(capture, [failed(one)])
      yield* ran(capture, [passed(one)], "full", LANE, TASK_ENV)

      const standing = yield* standingOf(capture)
      expect(standing?.runId).toBe(current.id)
      expect(standing?.failed).toBe(1)
      expect(standing?.caseRuns.map((entry) => entry.runId)).toEqual([current.id])
    })))

  it("never moves on a current-population run made on a dirty tree", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const capture = "identity_dirty.pcap.gz"
      const one = `${capture}-case1`
      const clean = yield* ran(capture, [failed(one)])
      const dirty = yield* runs.open({
        capture,
        lane: LANE,
        mode: "record",
        sessionDir: `/w/${capture}`,
        identity: CURRENT,
        provenance: [
          { name: "app", sha: "a".repeat(40), dirty: true, tree: "t1" },
          { name: "core", sha: "b".repeat(40), dirty: false, tree: "u1" }
        ]
      })
      yield* runs.finish({ id: dirty.id, status: "done", cases: [passed(one)] })

      const standing = yield* standingOf(capture)
      expect(standing?.runId).toBe(clean.id)
      expect(standing?.failed).toBe(1)
    })))

  it("reads a run of no recorded env as unknown, never as @current", () =>
    withRuns(Effect.gen(function*() {
      const capture = "identity_unknown.pcap.gz"
      yield* ran(capture, [passed(`${capture}-auto`)], "full", LANE, null)
      expect(yield* standingOf(capture)).toBeNull()
    })))

  it("reads as current the env and branch the store states, and nothing where it states none", () =>
    withRuns(Effect.gen(function*() {
      const sql = yield* SqlClient
      const current = yield* Effect.provide(Current.Service, Current.layer)
      const capture = "identity_setting.pcap.gz"
      const task = yield* ran(capture, [passed(`${capture}-auto`)], "full", LANE, TASK_ENV)
      expect(yield* standingOf(capture)).toBeNull()
      yield* current.set({ env: TASK_ENV.env!, branch: TASK_ENV.branch! })
      expect(yield* current.get).toEqual({ env: TASK_ENV.env, branch: TASK_ENV.branch })
      expect((yield* standingOf(capture))?.runId).toBe(task.id)
      yield* sql`DELETE FROM current_population`
      expect(yield* current.get).toBeNull()
      expect(yield* standingOf(capture)).toBeNull()
      yield* current.set(CURRENT_POPULATION)
    })))

  it("records the env, branch, lane hash and load on the row", () =>
    withRuns(Effect.gen(function*() {
      const sql = yield* SqlClient
      const capture = "identity_columns.pcap.gz"
      const opened = yield* ran(capture, [passed(`${capture}-auto`)], "full", LANE, {
        env: "dev-2-columns",
        branch: "fix/2-columns",
        laneHash: "abcdef0123456789",
        load: { concurrency: 4, batchSize: 100, startsPerSecond: 2.5, maxSpanSeconds: 120, memoryMax: "2G" }
      })
      const rows = yield* sql<Record<string, unknown>>`
        SELECT env, branch, lane_hash, concurrency, batch_size, starts_per_second, max_span_seconds, memory_max
        FROM runs WHERE id = ${opened.id}
      `
      expect(rows[0]).toEqual({
        env: "dev-2-columns",
        branch: "fix/2-columns",
        lane_hash: "abcdef0123456789",
        concurrency: 4,
        batch_size: 100,
        starts_per_second: 2.5,
        max_span_seconds: 120,
        memory_max: "2G"
      })
    })))
})

describe.skipIf(!served)("a call stands at its latest @current run", () => {
  it("moves the call a cases run replayed and leaves the others at the full run", () =>
    withRuns(Effect.gen(function*() {
      const capture = "caseset_red.pcap.gz"
      const one = `${capture}-case1`
      const two = `${capture}-case2`
      const green = yield* ran(capture, [passed(one), passed(two)])
      yield* ran(capture, [failed(one), failed(two)])
      const probe = yield* ran(capture, [passed(one)], "cases")

      const standing = yield* standingOf(capture)
      // The probe is the capture's latest run; case two still fails at the full run.
      expect(standing?.runId).toBe(probe.id)
      expect(standing?.failed).toBe(1)
      expect(standing?.caseIds).toEqual([one, two])
      expect(standing?.lastOkRunId).toBe(green.id)
    })))

  it("turns a capture green once a cases run passes its every failing call", () =>
    withRuns(Effect.gen(function*() {
      const capture = "caseset_green.pcap.gz"
      const one = `${capture}-case1`
      const two = `${capture}-case2`
      yield* ran(capture, [failed(one), passed(two)])
      yield* ran(capture, [passed(one)], "cases")

      const standing = (yield* standingOf(capture))!
      expect(standing.failed).toBe(0)
      expect(standing.cases).toBe(2)
      expect(isGreen(standing)).toBe(true)
    })))

  it("bounds the capture's cases by its latest full reading", () =>
    withRuns(Effect.gen(function*() {
      const capture = "caseset_recut.pcap.gz"
      const one = `${capture}-case1`
      const two = `${capture}-case2`
      yield* ran(capture, [failed(one), failed(two)])
      const recut = yield* ran(capture, [passed(one)])

      const standing = (yield* standingOf(capture))!
      expect(standing.caseIds).toEqual([one])
      expect(standing.caseRuns.map((entry) => entry.runId)).toEqual([recut.id])
    })))

  it("names the cases run as the case's current run", () =>
    withRuns(Effect.gen(function*() {
      const capture = "caseset_own.pcap.gz"
      const one = `${capture}-case1`
      const two = `${capture}-case2`
      const full = yield* ran(capture, [failed(one), failed(two)])
      const probe = yield* ran(capture, [passed(one)], "cases")

      const standing = yield* standingOf(capture)
      // Case one was last run by the probe, and passed there; case two only by the full run.
      expect(standing?.caseRuns).toEqual([
        { caseId: one, runId: probe.id, setKind: "cases", passed: true, skipped: null, bundlePath: probe.bundlePath },
        { caseId: two, runId: full.id, setKind: "full", passed: false, skipped: null, bundlePath: full.bundlePath }
      ])
    })))

  it("carries a case the cases run SKIPPED as skipped, not as failed", () =>
    withRuns(Effect.gen(function*() {
      const capture = "caseset_skipped.pcap.gz"
      const one = `${capture}-case1`
      yield* ran(capture, [failed(one)])
      const probe = yield* ran(capture, [{ ...passed(one), passed: false, skipped: "blocked" }], "cases")

      const standing = yield* standingOf(capture)
      expect(standing?.caseRuns).toEqual([
        { caseId: one, runId: probe.id, setKind: "cases", passed: false, skipped: "blocked", bundlePath: probe.bundlePath }
      ])
    })))

  it("reads a case's current run on the lane asked for, never across lanes", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const capture = "caseset_lanes.pcap.gz"
      const one = `${capture}-case1`
      const full = yield* ran(capture, [failed(one)], "full", "lane-a")
      const probe = yield* ran(capture, [passed(one)], "cases", "lane-b")

      const onA = yield* runs.standing(capture, "lane-a")
      expect(onA?.caseRuns.map((entry) => [entry.runId, entry.setKind])).toEqual([[full.id, "full"]])
      const onB = yield* runs.standing(capture, "lane-b")
      expect(onB?.caseRuns.map((entry) => [entry.runId, entry.setKind])).toEqual([[probe.id, "cases"]])
    })))

  it("lets a later full run take a case back from a cases run, and its bundle with it", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const capture = "caseset_taken_back.pcap.gz"
      const one = `${capture}-case1`
      yield* ran(capture, [failed(one)])
      const probe = yield* ran(capture, [passed(one)], "cases")
      const again = yield* ran(capture, [failed(one)])

      const standing = yield* standingOf(capture)
      expect(standing?.caseRuns.map((entry) => entry.runId)).toEqual([again.id])
      const kept = yield* runs.protectedBundles
      expect(kept.has(again.bundlePath)).toBe(true)
      expect(kept.has(probe.bundlePath)).toBe(false)
    })))

  it("keeps a cases run's bundle while it is a case's current run", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const capture = "caseset_keep.pcap.gz"
      const one = `${capture}-case1`
      const two = `${capture}-case2`
      const full = yield* ran(capture, [failed(one), failed(two)])
      const probe = yield* ran(capture, [passed(one)], "cases")

      const kept = yield* runs.protectedBundles
      // The full run is still case two's current run; the probe is case one's.
      expect(kept.has(full.bundlePath)).toBe(true)
      expect(kept.has(probe.bundlePath)).toBe(true)
    })))

  it("never stands on an interrupted full run, which is still a case's last run for what it landed", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const capture = "standing_interrupted.pcap.gz"
      const whole = yield* ran(capture, [passed(`${capture}-auto`), passed(`${capture}-b`)])
      // A whole replay stopped after one cell: the row is `interrupted` with that cell alone.
      const stopped = yield* ran(capture, [failed(`${capture}-b`)], "full", LANE, CURRENT, "interrupted")
      const standing = (yield* standingOf(capture))!
      expect(standing.runId).toBe(whole.id)
      expect(standing.status).toBe("done")
      expect(standing.caseRuns.map((entry) => [entry.caseId, entry.runId, entry.passed])).toEqual([
        [`${capture}-auto`, whole.id, true],
        [`${capture}-b`, stopped.id, false]
      ])
      // Its bundle is kept for as long as it is a case's last run.
      expect([...yield* runs.protectedBundles]).toContain(stopped.bundlePath)
      // A capture whose only whole run was interrupted stands nowhere.
      const alone = "standing_interrupted_alone.pcap.gz"
      yield* ran(alone, [passed(`${alone}-auto`)], "full", LANE, CURRENT, "interrupted")
      expect(yield* standingOf(alone)).toBeNull()
    })))

  it("never stands on, nor bounds the cases by, a full run still in flight", () =>
    withRuns(Effect.gen(function*() {
      const sql = yield* SqlClient
      const runs = yield* Runs.Service
      const capture = "standing_in_flight.pcap.gz"
      const whole = yield* ran(capture, [failed(`${capture}-auto`)])
      // A whole replay opened and not finished: no case of it has landed yet.
      const flying = yield* runs.open({
        capture,
        lane: LANE,
        mode: "record",
        sessionDir: `/w/${capture}`,
        identity: CURRENT
      })
      const standing = (yield* standingOf(capture))!
      expect(standing.runId).toBe(whole.id)
      expect(standing.caseRuns.map((entry) => [entry.caseId, entry.runId])).toEqual([[`${capture}-auto`, whole.id]])
      const current = yield* sql<{ case_id: string; run_id: string }>`
        SELECT case_id, run_id FROM current_cells WHERE capture = ${capture}
      `
      expect(current.map((row) => [row.case_id, Number(row.run_id)])).toEqual([[`${capture}-auto`, whole.id]])
      yield* runs.finish({ id: flying.id, status: "done", cases: [passed(`${capture}-auto`)] })
      expect((yield* standingOf(capture))?.runId).toBe(flying.id)
    })))

  it("records the set kind on the row, full by default", () =>
    withRuns(Effect.gen(function*() {
      const sql = yield* SqlClient
      const capture = "caseset_column.pcap.gz"
      const full = yield* ran(capture, [passed(`${capture}-auto`)])
      const probe = yield* ran(capture, [passed(`${capture}-auto`)], "cases")
      const rows = yield* sql<{ id: string; set_kind: string }>`
        SELECT id, set_kind FROM runs WHERE capture = ${capture} ORDER BY id
      `
      expect(rows.map((row) => [Number(row.id), row.set_kind])).toEqual([
        [full.id, "full"],
        [probe.id, "cases"]
      ])
    })))
})

describe.skipIf(!served)("many runs opened at once", () => {
  it("assigns ids in the order given, each row naming its own set and bundle", () =>
    withRuns(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const sql = yield* SqlClient
      const captures = Array.from({ length: 50 }, (_, at) => `many_${String(at).padStart(3, "0")}.pcap.gz`)
      const opened = yield* runs.openMany(
        captures.map((capture, at) => ({
          capture,
          lane: LANE,
          mode: "record",
          sessionDir: `/w/${capture}`,
          setKind: at % 2 === 0 ? "full" : "cases"
        }))
      )
      expect(opened).toHaveLength(50)
      // Ids climb with the list: the recency `standings` reads is the order asked for.
      for (let at = 1; at < opened.length; at++) expect(opened[at]!.id).toBeGreaterThan(opened[at - 1]!.id)
      for (const [at, run] of opened.entries()) {
        expect(run.name).toBe(`run${run.id}`)
        expect(run.bundlePath).toBe(`/w/${captures[at]}/run${run.id}`)
      }
      const rows = yield* sql<{ id: string; capture: string; set_kind: string; bundle_path: string }>`
        SELECT id, capture, set_kind, bundle_path FROM runs WHERE capture LIKE 'many_%' ORDER BY id
      `
      expect(rows.map((row) => [Number(row.id), row.capture, row.set_kind, row.bundle_path])).toEqual(
        opened.map((run, at) => [run.id, captures[at], at % 2 === 0 ? "full" : "cases", `${captures[at]}/run${run.id}`])
      )
    })))
})
