/**
 * What a selector names in the store against a real postgres:
 * the runs each term admits, two sides read narrowed to each other — each
 * call at the side's latest run holding it, no older than its latest full
 * reading — and a comparison read off them. The suite builds and drops its OWN
 * database and skips where no server answers.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import { compare, unexplained } from "../src/compare.js"
import type { CaseResult, Provenance } from "../src/model.js"
import type { RunIdentity } from "../src/run-identity.js"
import * as RunSets from "../src/run-sets.js"
import * as Runs from "../src/runs.js"
import * as Selector from "../src/selector.js"
import * as Sessions from "../src/sessions.js"
import * as Sweeps from "../src/sweeps.js"
import { CURRENT } from "./identity.js"

const SCRATCH = await Server.scratch("store_run_sets_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withStore = <A, E>(effect: Effect.Effect<A, E, Runs.Service | RunSets.Service | Sweeps.Service>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([Runs.layer, RunSets.layer, Sweeps.layer]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

const ENV: RunIdentity = { env: "dev-7-fix", branch: "fix/7-fix", laneHash: "lane1", load: null }

/** The commits each tree fingerprint was taken on; `t3` is a commit sharing `t2`'s first seven digits. */
const SHAS: Record<string, string> = { t1: "aaaa0000aa", t2: "bbbb0000bb", t3: "bbbb000cc1" }

/** The code a run ran: `tree` is the app's fingerprint, `dirty` whether the app tree was. */
const code = (tree: string, dirty = false): Provenance => [
  { name: "app", sha: SHAS[tree]!, dirty, tree },
  { name: "core", sha: "beef0000ee", dirty: false, tree: "u1" }
]

const result = (caseId: string, passed: boolean, over: Partial<CaseResult> = {}): CaseResult => ({
  caseId,
  passed,
  verdict: passed ? "ok" : "failed",
  skipped: null,
  failedStep: passed ? null : "s6",
  rfcGating: 0,
  mainError: null,
  classification: { accepted: 2, known_bug: 0, unlisted: 0, unknown: 0 },
  document: { id: caseId },
  ...over
})

const ran = (
  capture: string,
  lane: string,
  identity: RunIdentity,
  tree: string,
  cases: ReadonlyArray<CaseResult>,
  over: { readonly sweepId?: number; readonly dirty?: boolean; readonly setKind?: "full" | "cases" } = {}
) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const opened = yield* runs.open({
      capture,
      lane,
      mode: "record",
      sessionDir: `/w/${capture}`,
      identity,
      setKind: over.setKind ?? "full",
      provenance: code(tree, over.dirty ?? false),
      ...(over.sweepId === undefined ? {} : { sweepId: over.sweepId })
    })
    yield* runs.finish({ id: opened.id, status: "done", cases })
    return opened.id
  })

const parse = (text: string): Selector.Selector => {
  const parsed = Selector.parse(text)
  if (Selector.isRefused(parsed)) throw new Error(parsed.refused)
  return parsed
}

const A = "sets_a.pcap.gz"
const B = "sets_b.pcap.gz"
/** Cut whole twice: the re-cut refuses case9, which the first reading held. */
const R = "sets_recut.pcap.gz"
/** Run by the current population on a dirty tree only. */
const D = "sets_dirty.pcap.gz"
/** Run on a third commit sharing the env's first seven digits. */
const E = "sets_twin.pcap.gz"

let ids: Record<string, number> = {}

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
  ids = await withStore(Effect.gen(function*() {
    const sweeps = yield* Sweeps.Service
    const sweep = yield* sweeps.open({
      lane: "fake",
      universe: "named",
      selection: `${A} ${B}`,
      captures: 2,
      concurrency: 8,
      batchSize: 1000,
      memoryMax: "4G"
    })
    const currentA = yield* ran(A, "fake", CURRENT, "t1", [result(`${A}-case1`, false), result(`${A}-case2`, true)], {
      sweepId: sweep
    })
    const currentB = yield* ran(B, "fake", CURRENT, "t1", [result(`${B}-auto`, true)], { sweepId: sweep })
    const refA = yield* ran(A, "ref", CURRENT, "t1", [result(`${A}-case1`, true)])
    const currentR = yield* ran(R, "fake", CURRENT, "t1", [result(`${R}-case1`, false), result(`${R}-case9`, true)])
    const recutR = yield* ran(R, "fake", CURRENT, "t1", [result(`${R}-case1`, false)])
    const dirtyD = yield* ran(D, "fake", CURRENT, "t1", [result(`${D}-auto`, true)], { dirty: true })
    const envA = yield* ran(A, "fake", ENV, "t2", [result(`${A}-case1`, true)])
    const envAgain = yield* ran(A, "fake", ENV, "t2", [result(`${A}-case2`, false)], { setKind: "cases" })
    const envR = yield* ran(R, "fake", ENV, "t2", [result(`${R}-case1`, false), result(`${R}-case9`, false)])
    const envD = yield* ran(D, "fake", ENV, "t2", [result(`${D}-auto`, true)])
    const twinE = yield* ran(E, "fake", ENV, "t3", [result(`${E}-auto`, true)])
    return { sweep, currentA, currentB, refA, currentR, recutR, dirtyD, envA, envAgain, envR, envD, twinE }
  }))
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

const runsOf = (text: string) =>
  withStore(Effect.flatMap(RunSets.Service, (sets) => sets.runs(parse(text), 100))).then((rows) =>
    rows.map((row) => row.id).sort((a, b) => a - b)
  )

const pairOf = (before: string, after: string) =>
  withStore(Effect.flatMap(RunSets.Service, (sets) => sets.pair(parse(before), parse(after), 100)))

const refusalOf = (text: string) => withStore(Effect.flatMap(RunSets.Service, (sets) => sets.refusal(parse(text))))

describe.skipIf(!served)("RunSets.runs", () => {
  it("admits the runs every term names", async () => {
    expect(await runsOf("@current")).toEqual([ids.currentA, ids.currentB, ids.refA, ids.currentR, ids.recutR])
    expect(await runsOf("@env:dev-7-fix")).toEqual([ids.envA, ids.envAgain, ids.envR, ids.envD, ids.twinE])
    expect(await runsOf("@current/@lane:ref")).toEqual([ids.refA])
    expect(await runsOf(`run${ids.currentB}`)).toEqual([ids.currentB])
    expect(await runsOf("@sha:bbbb0000")).toEqual([ids.envA, ids.envAgain, ids.envR, ids.envD])
    expect(await runsOf(`@sweep:${ids.sweep}`)).toEqual([ids.currentA, ids.currentB])
  })

  it("leaves a run made on a dirty tree out of @current and out of @sha", async () => {
    expect(await runsOf("@current")).not.toContain(ids.dirtyD)
    expect(await runsOf("@sha:aaaa000")).not.toContain(ids.dirtyD)
    expect(await runsOf(`run${ids.dirtyD}`)).toEqual([ids.dirtyD])
  })
})

describe.skipIf(!served)("RunSets.refusal", () => {
  it("refuses a commit prefix naming two commits, listing them, and answers one naming one", async () => {
    expect(await refusalOf("@sha:bbbb000")).toBe(`@sha:bbbb000 names 2 commits — ${SHAS["t2"]}, ${SHAS["t3"]}`)
    expect(await refusalOf("@sha:bbbb0000")).toBeNull()
    expect(await refusalOf("@current")).toBeNull()
  })

  it("refuses the current term where the store states no current population", async () => {
    const stated = await on(SCRATCH, Effect.flatMap(SqlClient, (sql) => sql<{ env: string; branch: string }>`
      DELETE FROM current_population RETURNING env, branch
    `))
    try {
      expect(await refusalOf("@current/@lane:ref")).toMatch(/states no current population/)
      expect(await refusalOf("@lane:ref")).toBeNull()
    } finally {
      await on(SCRATCH, Effect.flatMap(SqlClient, (sql) => sql`
        INSERT INTO current_population (env, branch) VALUES (${stated[0]!.env}, ${stated[0]!.branch})
      `))
    }
  })
})

describe.skipIf(!served)("RunSets.pair", () => {
  it("reads each call at the side's latest run holding it", async () => {
    const { before } = await pairOf("@env:dev-7-fix", "@env:dev-7-fix")
    expect(before.cells.filter((cell) => cell.capture === A).map((cell) => [cell.caseId, cell.runId, cell.passed]))
      .toEqual([
        [`${A}-case1`, ids.envA, true],
        [`${A}-case2`, ids.envAgain, false]
      ])
    expect(before.runs).toBe(5)
  })

  it("carries the population each sweep of the side stated", async () => {
    const { before } = await pairOf(`@sweep:${ids.sweep}`, `@sweep:${ids.sweep}`)
    expect(before.sweeps).toEqual([{ id: ids.sweep, universe: "named", selection: `${A} ${B}`, captures: 2 }])
  })

  it("reads a side no older than its latest full reading, as the current cells do", async () => {
    const { before, after } = await pairOf("@current/@lane:fake", "@env:dev-7-fix")
    // The current re-cut no longer holds case9: neither does the current side.
    expect(before.cells.filter((cell) => cell.capture === R).map((cell) => [cell.caseId, cell.runId])).toEqual([
      [`${R}-case1`, ids.recutR]
    ])
    expect(after.elsewhere.keys).toContain(`${R}/${R}-case9`)
  })

  it("reads a side as of its latest FINISHED full reading, never one still in flight", async () => {
    const capture = "sets_in_flight.pcap.gz"
    const lane = "in-flight"
    const finished = await withStore(ran(capture, lane, CURRENT, "t1", [result(`${capture}-case1`, false)]))
    await withStore(Effect.flatMap(Runs.Service, (runs) =>
      runs.open({ capture, lane, mode: "record", sessionDir: `/w/${capture}`, identity: CURRENT, provenance: code("t1") })))
    const { before } = await pairOf(`@current/@lane:${lane}`, `@current/@lane:${lane}`)
    expect(before.cells.map((cell) => [cell.caseId, cell.runId])).toEqual([[`${capture}-case1`, finished]])
  })

  it("loads only the calls both sides hold, and counts the captures one side alone ran", async () => {
    const { before, after } = await pairOf("@current/@lane:fake", "@env:dev-7-fix")
    expect([...new Set(before.cells.map((cell) => cell.capture))].sort()).toEqual([A, R])
    expect(before.cells.map((cell) => `${cell.capture}/${cell.caseId}`).sort())
      .toEqual(after.cells.map((cell) => `${cell.capture}/${cell.caseId}`).sort())
    // B alone on current; D (current ran it only on a dirty tree) and E alone on the env.
    expect(before.elsewhere).toEqual({ captures: 1, calls: 0, keys: [] })
    expect(after.elsewhere).toEqual({ captures: 2, calls: 1, keys: [`${R}/${R}-case9`] })
  })
})

describe.skipIf(!served)("compare over two sides", () => {
  it("classes the calls both sides ran and counts what one side alone holds", async () => {
    const { before, after } = await pairOf("@current/@lane:fake", "@env:dev-7-fix")
    const comparison = compare(before, after)
    expect(comparison.moves.map((move) => [move.caseId, move.class, move.code])).toEqual([
      [`${A}-case2`, "green→red", "different"],
      [`${A}-case1`, "red→green", "different"],
      [`${R}-case1`, "unchanged", "different"]
    ])
    expect(comparison.onlyBefore).toEqual({ captures: 1, calls: 0, keys: [] })
    expect(comparison.onlyAfter.calls).toBe(1)
  })

  it("compares two lanes as two sides", async () => {
    const { before, after } = await pairOf("@current/@lane:fake", "@current/@lane:ref")
    const comparison = compare(before, after)
    expect(comparison.moves.map((move) => [move.caseId, move.class, move.code, move.lane])).toEqual([
      [`${A}-case1`, "red→green", "same", "unknown"]
    ])
    // Two lanes are two behaviours: a flip across them is never the harness's.
    expect(unexplained(comparison)).toBe(0)
  })
})

describe.skipIf(!served)("repeats within a side", () => {
  const P = "sets_repeat.pcap.gz"
  const BASE: RunIdentity = { env: "dev-8-base", branch: "fix/8-base", laneHash: "lane1", load: null }
  const REPEAT: RunIdentity = { env: "dev-8-repeat", branch: "fix/8-repeat", laneHash: "lane1", load: null }

  it("reads how many of a side's runs of a call on its code, document and lane passed, and classes a split unstable", async () => {
    await withStore(Effect.gen(function*() {
      yield* ran(P, "fake", BASE, "t1", [result(`${P}-case1`, false)])
      yield* ran(P, "fake", REPEAT, "t1", [result(`${P}-case1`, true)], { setKind: "cases" })
      yield* ran(P, "fake", REPEAT, "t1", [result(`${P}-case1`, false)], { setKind: "cases" })
      // Another tree's run is no repeat of the latest.
      yield* ran(P, "fake", REPEAT, "t2", [result(`${P}-case1`, false)], { setKind: "cases" })
      yield* ran(P, "fake", REPEAT, "t1", [result(`${P}-case1`, false)], { setKind: "cases" })
    }))
    const { before, after } = await pairOf("@env:dev-8-base", "@env:dev-8-repeat")
    expect(before.cells.map((cell) => cell.repeats)).toEqual([{ runs: 1, passed: 0 }])
    expect(after.cells.map((cell) => cell.repeats)).toEqual([{ runs: 3, passed: 1 }])
    expect(compare(before, after).moves.map((move) => move.class)).toEqual(["unstable"])
  })

  it("reads no repeats where the lane hash is unknown: runs of an unknown lane behaviour repeat nothing", async () => {
    const Q = "sets_repeat_unhashed.pcap.gz"
    const UNHASHED: RunIdentity = { env: "dev-9-unhashed", branch: "fix/9", laneHash: null, load: null }
    const BASE9: RunIdentity = { env: "dev-9-base", branch: "fix/9-base", laneHash: null, load: null }
    await withStore(Effect.gen(function*() {
      yield* ran(Q, "real", BASE9, "t1", [result(`${Q}-case1`, false)])
      yield* ran(Q, "real", UNHASHED, "t1", [result(`${Q}-case1`, true)], { setKind: "cases" })
      yield* ran(Q, "real", UNHASHED, "t1", [result(`${Q}-case1`, false)], { setKind: "cases" })
    }))
    const { before, after } = await pairOf("@env:dev-9-base", "@env:dev-9-unhashed")
    expect(after.cells.map((cell) => cell.repeats)).toEqual([undefined])
    expect(compare(before, after).moves.map((move) => move.class)).toEqual(["unchanged"])
  })
})
