/**
 * The campaign record against a real postgres: a campaign run's lanes, their
 * case results and the failed checks their verification lines state read back
 * as written, each result as its outcome; the target and what each case says
 * of itself read back as the pack wrote them, the target merged key by key;
 * every attempt of a retried cell and the kept cells of a lane's bundle; the
 * latest runs of one environment; the result reset emptying the record. The
 * suite builds and drops its own database.
 */
import type { Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as CampaignResults from "../src/campaign-results.js"
import * as CampaignRuns from "../src/campaign-runs.js"
import * as Client from "../src/client.js"
import type * as Model from "../src/model.js"
import * as ResultReset from "../src/result-reset.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_campaign_runs_test")
const CAMPAIGN = "campaign"

type Services = CampaignRuns.Service | ResultReset.Service | Runs.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([CampaignRuns.layer, ResultReset.layer, Runs.layer]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const result = (caseId: string, rest: Partial<Model.CaseResult>): Model.CaseResult => ({
  caseId,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null,
  ...rest
})

const NO_SELECTION = { allow: [], lanes: {} }

/** A run of `capture` opened with nothing but its directory and scope. */
const bare = (capture: string, dir: string, scope: string | null = null, env: string | null = null) => ({
  capture,
  env,
  branch: null,
  seed: null,
  dir,
  scope,
  selection: NO_SELECTION,
  target: {}
})

const ROUTE = CampaignResults.caseIdOf("cases/route/incoming", "caller_bye")
const REJECT = CampaignResults.caseIdOf("cases/reject/outgoing", "immediate")
const REFUSED = CampaignResults.caseIdOf("cases/limit/one", "caller_bye+immediate")
const UNPORTED = CampaignResults.caseIdOf("cases/other/unported")

const PAI: CampaignResults.Check = {
  anchor: "B1 INVITE",
  field: "pai.uri",
  expected: "sip:+15550100@example.com;user=phone",
  seen: "sip:+15550100@127.0.0.1;user=phone",
  said: "pai = <sip:+15550100@example.com;user=phone>",
  side: "sut",
  compare: "value",
  step: "s3",
  failure: 0
}
const REAPED: CampaignResults.Check = {
  anchor: "post-run cleanup",
  field: "sut.reaped",
  expected: "every call removed",
  seen: null,
  said: null,
  side: null,
  compare: "statement",
  failure: 1
}

/** `check` as a failing line of verification `name`. */
const failing = (name: string, check: CampaignResults.Check): Verification.Verification => ({
  name,
  source: name === "call" ? "interpreter" : "lane gate",
  answer: "fail",
  message: `${check.anchor} ${check.field}`,
  detail: CampaignResults.detailOf(check)
})

beforeAll(async () => {
  if (!Server.served) return
  await Server.fresh(SCRATCH)
  await Server.on(SCRATCH, Server.migrated)
}, 60_000)

afterAll(async () => {
  if (!Server.served) return
  await Server.drop(SCRATCH)
}, 60_000)

describe.skipIf(!Server.served)("the campaign record", () => {
  it("reads a run's lanes, results and failed checks back, each result as its outcome", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const store = yield* CampaignRuns.Service
      const selection = { allow: ["very-long"], lanes: { real: ["stops-service"], fake: [] } }
      const id = yield* store.open({
        capture: CAMPAIGN,
        env: "dev-test",
        branch: "feat/x",
        seed: "seed-1",
        dir: "/runs/one",
        scope: null,
        selection,
        target: { version: "backend:v1.2.3" }
      })
      const fake = yield* runs.open({ capture: CAMPAIGN, lane: "fake", mode: "provisioned", sessionDir: `/w/${CAMPAIGN}` })
      const real = yield* runs.open({ capture: CAMPAIGN, lane: "real", mode: "provisioned", sessionDir: `/w/${CAMPAIGN}` })
      yield* store.link(id, fake.id)
      yield* store.link(id, real.id)
      yield* runs.finish({
        id: fake.id,
        status: "done",
        cases: [
          result(ROUTE, {
            passed: false,
            verdict: "failed",
            verifications: [failing("call", PAI), failing("cleanup", REAPED)]
          }),
          result(REJECT, {
            verifications: [{ name: "call", source: "interpreter", answer: "pass" }, { ...failing("call", PAI), waived: true }]
          }),
          result(REFUSED, {
            passed: false,
            verdict: "absent",
            mainError: { kind: "refused", rule: CampaignResults.ERROR_RULE, rfc: null, step: null, expected: null, detail: "past the cap" }
          }),
          result(UNPORTED, { passed: false, verdict: "absent", skipped: CampaignResults.NOT_PORTED })
        ]
      })
      yield* store.writeCell(fake.id, ROUTE, { cell: "route--caller_bye", log: { records: 4 } })
      yield* store.writeCell(fake.id, REFUSED, { cell: "limit", log: { missing: "no record names r1" } })
      yield* runs.finish({ id: real.id, status: "error", detail: "provision: database init failed", cases: [] })
      yield* store.finish(id)

      const [read] = yield* store.read(CAMPAIGN, { limit: 1 })
      expect([read!.id, read!.seed, read!.selection, read!.target, read!.about]).toEqual([
        id,
        "seed-1",
        selection,
        { version: "backend:v1.2.3" },
        {}
      ])
      expect(read!.finishedAt).not.toBeNull()
      expect(read!.lanes.map((lane) => [lane.lane, lane.status, lane.detail, lane.bundlePath])).toEqual([
        ["fake", "done", null, `/w/${CAMPAIGN}/run${fake.id}`],
        ["real", "error", "provision: database init failed", `/w/${CAMPAIGN}/run${real.id}`]
      ])
      const results = read!.lanes[0]!.results
      expect(Object.fromEntries(results.map((r) => [r.caseId, CampaignResults.outcomeOf(r)]))).toEqual({
        [ROUTE]: "failure",
        [REJECT]: "pass",
        [REFUSED]: "error",
        [UNPORTED]: "skipped"
      })
      const byId = new Map(results.map((r) => [r.caseId, r]))
      expect(byId.get(ROUTE)!.checks).toEqual([PAI, REAPED])
      expect(byId.get(ROUTE)!.verifications.map((line) => [line.name, line.answer])).toEqual([
        ["call", "fail"],
        ["cleanup", "fail"]
      ])
      expect(byId.get(REJECT)!.checks).toEqual([])
      expect(byId.get(UNPORTED)!.verifications).toEqual([])
      expect([byId.get(ROUTE)!.cell, byId.get(ROUTE)!.log]).toEqual(["route--caller_bye", { records: 4 }])
      expect(byId.get(REFUSED)!.log).toEqual({ missing: "no record names r1" })
      expect([byId.get(UNPORTED)!.cell, byId.get(UNPORTED)!.log]).toEqual([null, null])
      expect(yield* store.captures).toContain(CAMPAIGN)
    })))

  it("merges the target key by key and replaces what each case says of itself", () =>
    withStore(Effect.gen(function*() {
      const store = yield* CampaignRuns.Service
      const id = yield* store.open({ ...bare("about", "/d"), target: { version: "v1", console: "http://127.0.0.1:1" } })
      const about = { title: "A call from an unknown trunk is refused", uses: ["trunk"] }
      yield* store.describe(id, { "cases/old": about })
      yield* store.describe(id, { [REJECT]: about })
      yield* store.target(id, { console: "http://127.0.0.1:2", objects: [{ name: "vpn", id: 1 }] })
      const [read] = yield* store.read("about", { ids: [id] })
      expect(read!.about).toEqual({ [REJECT]: about })
      expect(read!.target).toEqual({ version: "v1", console: "http://127.0.0.1:2", objects: [{ name: "vpn", id: 1 }] })
    })))

  it("reads back every attempt of a cell replayed more than once, the kept one marked, and the kept cells of a bundle", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const store = yield* CampaignRuns.Service
      const id = yield* store.open(bare(CAMPAIGN, "/runs/three", "retried"))
      const lane = yield* runs.open({ capture: CAMPAIGN, lane: "fake", mode: "provisioned", sessionDir: `/w/${CAMPAIGN}` })
      yield* store.link(id, lane.id)
      yield* runs.finish({ id: lane.id, status: "done", cases: [result(ROUTE, {}), result(REJECT, {})] })
      const attempts: ReadonlyArray<CampaignResults.Attempt> = [
        { cell: "route--caller_bye", outcome: "failure", failed: 2, kept: false },
        { cell: null, outcome: "error", failed: 0, kept: false },
        { cell: "route--caller_bye~3", outcome: "pass", failed: 0, kept: true }
      ]
      yield* store.writeCell(lane.id, ROUTE, { cell: "route--caller_bye", log: null })
      yield* store.writeCell(lane.id, ROUTE, { cell: "route--caller_bye~3", log: null, attempts })
      yield* store.writeCell(lane.id, REJECT, { cell: "reject--immediate", log: null })
      const [read] = yield* store.read(CAMPAIGN, { ids: [id] })
      const byId = new Map(read!.lanes[0]!.results.map((r) => [r.caseId, r]))
      expect([byId.get(ROUTE)!.cell, byId.get(ROUTE)!.attempts]).toEqual(["route--caller_bye~3", attempts])
      expect(byId.get(REJECT)!.attempts).toBeUndefined()
      expect(read!.scope).toBe("retried")
      expect(yield* store.keptCells(lane.bundlePath)).toEqual([
        { cell: "reject--immediate", runId: lane.id, caseId: REJECT, lane: "fake" },
        { cell: "route--caller_bye~3", runId: lane.id, caseId: ROUTE, lane: "fake" }
      ])
      expect(yield* store.keptCells(`/w/${CAMPAIGN}/no-such-run`)).toEqual([])
      expect(yield* store.keptCells("/elsewhere/run1")).toEqual([])
    })))

  it("reads one environment's latest runs, an older run asked for among them, full or partial, and names every writer", () =>
    withStore(Effect.gen(function*() {
      const store = yield* CampaignRuns.Service
      const open = (env: string, scope: string | null = null) => store.open(bare("older", "/d", scope, env))
      const old = yield* open("env-a")
      yield* open("env-b")
      const latest = yield* open("env-a")
      const ids = (read: ReadonlyArray<CampaignRuns.StoredRun>) => read.map((r) => r.id)
      expect(ids(yield* store.read("older", { limit: 1, env: "env-a" }))).toEqual([latest])
      expect(ids(yield* store.read("older", { limit: 1, env: "env-a", include: old }))).toEqual([latest, old])
      expect(yield* store.envs("older")).toEqual(["env-a", "env-b"])
      const partial = yield* open("env-a", "cases/x [a]")
      expect(ids(yield* store.read("older", { limit: 1, env: "env-a", partial: false }))).toEqual([latest])
      expect(ids(yield* store.read("older", { limit: 5, env: "env-a", partial: true }))).toEqual([partial])
    })))

  it("is emptied whole by the result reset", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const store = yield* CampaignRuns.Service
      const reset = yield* ResultReset.Service
      const sql = yield* SqlClient
      const id = yield* store.open(bare("reset", "/runs/reset"))
      const lane = yield* runs.open({ capture: "reset", lane: "fake", mode: "provisioned", sessionDir: "/w/reset" })
      yield* store.link(id, lane.id)
      yield* store.describe(id, { [ROUTE]: { title: "t" } })
      yield* runs.finish({ id: lane.id, status: "done", cases: [result(ROUTE, {})] })
      yield* store.writeCell(lane.id, ROUTE, { cell: "c", log: null })
      yield* store.finish(id)
      const plan = yield* reset.execute({})
      expect(plan.emptied).toEqual(
        expect.arrayContaining(["campaign_case_about", "campaign_cells", "campaign_run_lanes", "campaign_runs"])
      )
      const [left] = yield* sql<Record<string, string>>`
        SELECT (SELECT count(*) FROM campaign_runs) AS runs, (SELECT count(*) FROM campaign_run_lanes) AS lanes,
               (SELECT count(*) FROM campaign_cells) AS cells, (SELECT count(*) FROM campaign_case_about) AS about
      `
      expect(left).toEqual({ runs: "0", lanes: "0", cells: "0", about: "0" })
    })))
})
