/**
 * The diff of two campaign runs. Over two sides stated by hand: a cell whose
 * status moved, one whose lines alone moved, one only a side holds and a
 * count that moved are each named under their lane, the verdict last; two
 * alike sides read `same`. Against a real postgres: a side's counts are its
 * cells', batches' and job's attachments and lines and its backend log's
 * records, a job counted once over two lanes, and a run the store lacks is
 * refused by its id. The suite builds and drops its own database.
 */
import type { Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import type { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Attachments from "../src/attachments.js"
import * as Batches from "../src/batches.js"
import * as CampaignDiff from "../src/campaign-diff.js"
import type * as CampaignResults from "../src/campaign-results.js"
import * as CampaignRuns from "../src/campaign-runs.js"
import * as Client from "../src/client.js"
import * as Jobs from "../src/jobs.js"
import type * as Model from "../src/model.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import * as Verifications from "../src/verifications.js"
import * as Server from "./server.js"

const pass = (name: string, source: string): Verification.Verification => ({ name, source, answer: "pass" })
const FROM: Verification.Verification = {
  name: "call",
  source: "interpreter",
  answer: "fail",
  detail: { anchor: "B1 INVITE", field: "from.uri" }
}

const cell = (caseId: string, verifications: ReadonlyArray<Verification.Verification>): CampaignResults.Result => ({
  caseId,
  cell: caseId,
  log: null,
  passed: verifications.every((line) => line.answer === "pass"),
  skipped: null,
  verdict: "ok",
  errorKind: null,
  errorRule: null,
  errorDetail: null,
  checks: [],
  verifications
})

const run = (id: number, results: ReadonlyArray<CampaignResults.Result>): CampaignResults.CampaignRun => ({
  id,
  capture: "campaign",
  env: "integration",
  branch: "develop",
  seed: null,
  dir: `/runs/${id}`,
  scope: null,
  selection: null,
  startedAt: "2026-01-02T03:04:05.000Z",
  finishedAt: "2026-01-02T03:05:05.000Z",
  lanes: [{ runId: id * 10, lane: "simulated", status: "done", detail: null, bundlePath: `/w/run${id}`, components: [], results }]
})

const GREEN = [pass("call", "interpreter"), pass("cleanup", "lane gate")]
const BEFORE: CampaignDiff.Side = {
  run: run(7, [cell("route [bye]", GREEN), cell("reject", GREEN), cell("hold", GREEN), cell("gone", GREEN)]),
  counts: [
    { lane: "simulated", what: "cell attachment sut-log", count: 4 },
    { lane: "simulated", what: "batch attachment pcap", count: 1 },
    { lane: null, what: "job attachment dump", count: 1 }
  ]
}
const AFTER: CampaignDiff.Side = {
  run: run(9, [
    cell("route [bye]", [FROM, pass("cleanup", "lane gate")]),
    cell("reject", [...GREEN, { ...pass("records", "collector"), informative: true }]),
    cell("hold", GREEN),
    cell("new", GREEN)
  ]),
  counts: [
    { lane: "simulated", what: "cell attachment sut-log", count: 3 },
    { lane: "simulated", what: "batch attachment pcap", count: 1 },
    { lane: null, what: "job attachment dump", count: 1 }
  ]
}

describe("the diff of two campaign runs", () => {
  it("names each cell that moved with the lines one side holds, each count that moved, the verdict last", () => {
    expect(CampaignDiff.render(CampaignDiff.diffOf(BEFORE, AFTER))).toEqual([
      "campaign run 7 (campaign, env integration, develop, 2026-01-02T03:04:05.000Z)",
      "     against 9 (campaign, env integration, develop, 2026-01-02T03:04:05.000Z)",
      "lane simulated: 3 cell(s) in both, 4 differ",
      "  only in 7  gone: pass",
      "  only in 9  new: pass",
      "  moved  reject: pass, lines moved",
      "      + records/collector=pass (informative)",
      "  moved  route [bye]: pass → fail",
      "      - call/interpreter=pass",
      "      + call/interpreter=fail [B1 INVITE from.uri]",
      "  captured  cell attachment sut-log: 4 → 3 (-1)",
      "  captured alike  batch attachment pcap 1",
      "job:",
      "  captured alike  job attachment dump 1",
      "DIFF moved (2 cell(s) moved, 2 on one side only, 1 count(s) moved; 3 cell(s) on 1 lane(s) in both)"
    ])
  })

  it("reads two alike sides as same, whatever their ids", () => {
    const again = { ...BEFORE, run: { ...BEFORE.run, id: 8 } }
    const diff = CampaignDiff.diffOf(BEFORE, again)
    expect(CampaignDiff.isSame(diff)).toBe(true)
    expect(CampaignDiff.render(diff).at(-1)).toBe("DIFF same (4 cell(s) on 1 lane(s) in both)")
  })

  it("names a line a cell held twice and holds once", () => {
    const twice = { ...BEFORE, run: run(7, [cell("hold", [...GREEN, pass("call", "interpreter")])]) }
    const once = { ...BEFORE, run: run(8, [cell("hold", GREEN)]) }
    expect(CampaignDiff.render(CampaignDiff.diffOf(twice, once)).slice(2, 5)).toEqual([
      "lane simulated: 1 cell(s) in both, 1 differ",
      "  moved  hold: pass, lines moved",
      "      - call/interpreter=pass"
    ])
  })

  it("reads a lane only one run drove as cells on one side only", () => {
    const other = { ...AFTER.run.lanes[0]!, lane: "real", results: [cell("hold", GREEN)] }
    const diff = CampaignDiff.diffOf(BEFORE, { run: { ...BEFORE.run, id: 8, lanes: [...BEFORE.run.lanes, other] }, counts: BEFORE.counts })
    expect(diff.lanes.map((lane) => [lane.lane, lane.shared, lane.moved.length])).toEqual([["real", 0, 1], ["simulated", 4, 0]])
    expect(CampaignDiff.isSame(diff)).toBe(false)
  })
})

const SCRATCH = await Server.scratch("store_campaign_diff_test")

type Services =
  | Attachments.Service
  | Batches.Service
  | CampaignRuns.Service
  | Jobs.Service
  | Runs.Service
  | Verifications.Service
  | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>) =>
  Effect.runPromiseExit(
    effect.pipe(
      Effect.provide([Attachments.layer, Batches.layer, CampaignRuns.layer, Jobs.layer, Runs.layer, Verifications.layer]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const result = (caseId: string): Model.CaseResult => ({
  caseId,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null,
  verifications: [pass("call", "interpreter")]
})

const file = (name: string) => ({ name, kind: "log" as const, file: `${name}.log`, source: "collector" })

beforeAll(async () => {
  if (!Server.served) return
  await Server.fresh(SCRATCH)
  await Server.on(SCRATCH, Server.migrated)
}, 60_000)

afterAll(async () => {
  if (!Server.served) return
  await Server.drop(SCRATCH)
}, 60_000)

describe.skipIf(!Server.served)("a side read from the store", () => {
  it("counts its cells', batches' and job's attachments and lines and its log records, the job once", async () => {
    const exit = await withStore(Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const runs = yield* Runs.Service
      const batches = yield* Batches.Service
      const attachments = yield* Attachments.Service
      const lines = yield* Verifications.Service
      const store = yield* CampaignRuns.Service
      const job = yield* jobs.create({ kind: "campaign", args: {}, createdBy: "test" })
      const id = yield* store.open({
        capture: "campaign",
        env: "integration",
        branch: null,
        seed: null,
        dir: "/runs/one",
        scope: null,
        selection: { allow: [], lanes: {} },
        target: {}
      })
      for (const lane of ["simulated", "real"]) {
        const opened = yield* runs.open({ capture: "campaign", lane, mode: "provisioned", sessionDir: "/w/campaign", jobId: job })
        yield* store.link(id, opened.id)
        yield* runs.finish({ id: opened.id, status: "done", cases: [result("one"), result("two")] })
        yield* store.writeCell(opened.id, "one", { cell: "one", log: { records: 5 } })
        yield* store.writeCell(opened.id, "two", { cell: "two", log: { missing: "no record" } })
        yield* attachments.add({ scope: "cell", runId: opened.id, caseId: "one" }, { dir: "/w/campaign/one", attachments: [file("sut-log")] })
        const batch = yield* batches.open({ jobId: job, lane, cells: ["one", "two"] })
        yield* batches.close(batch, null, [{ name: "lab-guard", source: "collector", answer: "pass" }])
        yield* attachments.add({ scope: "batch", batchId: batch }, { dir: "/w/campaign/batch", attachments: [file("unclaimed")] })
      }
      yield* attachments.add({ scope: "job", jobId: job }, { dir: "/w/campaign/job", attachments: [file("dump")] })
      yield* lines.writeJob(job, [{ name: "restart", source: "collector", answer: "pass" }])
      return { id, side: yield* CampaignDiff.load(id) }
    }))
    expect(Exit.isSuccess(exit)).toBe(true)
    if (!Exit.isSuccess(exit)) return
    expect(exit.value.side.run.id).toBe(exit.value.id)
    const counted = [...exit.value.side.counts].map((count) => `${count.lane ?? "job"}: ${count.what} ${count.count}`).sort()
    expect(counted).toEqual([
      "job: job attachment dump 1",
      "job: job line restart/collector=pass 1",
      ...["real", "simulated"].flatMap((lane) => [
        `${lane}: backend log missing 1`,
        `${lane}: backend log records 5`,
        `${lane}: batch attachment unclaimed 1`,
        `${lane}: batch line lab-guard/collector=pass 1`,
        `${lane}: cell attachment sut-log 1`
      ])
    ])
  })

  it("refuses a run the store does not hold, by its id", async () => {
    const exit = await withStore(CampaignDiff.load(987_654))
    expect(String(exit)).toContain("the store holds no campaign run 987654")
  })
})
