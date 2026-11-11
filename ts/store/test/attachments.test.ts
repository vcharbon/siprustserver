/**
 * Attachments against a real postgres: a run read off its bundle lands each
 * cell's attachments as the cell listed them, each read back as a path on this
 * box; a cell written again replaces them; a late writer adds by name; a batch
 * and a job hold their own; the result reset empties a cell's and a batch's
 * with them and keeps a job's, the job kept. The suite builds and drops its
 * own database.
 */
import { NodeServices } from "@effect/platform-node"
import { Attachment } from "@sip/contracts"
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Attachments from "../src/attachments.js"
import * as Batches from "../src/batches.js"
import * as Bundle from "../src/bundle.js"
import * as Client from "../src/client.js"
import * as Jobs from "../src/jobs.js"
import type { CaseResult } from "../src/model.js"
import * as ResultReset from "../src/result-reset.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import { CURRENT } from "./identity.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_attachments_test")
const TREE = fs.mkdtempSync(path.join(os.tmpdir(), "attachments-"))

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

type Services = Attachments.Service | Batches.Service | Jobs.Service | ResultReset.Service | Runs.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([Attachments.layer, Batches.layer, Jobs.layer, ResultReset.layer, Runs.layer]),
      Effect.provide(Sessions.layer(TREE)),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const LOG: Attachment.Attachment = { name: "sut-log", kind: "log", title: "the SUT's log", file: "sut.log", source: "replay" }
const RECORDS: Attachment.Attachment = { name: "call-records", kind: "records", file: "records.jsonl", source: "collector" }

const cell = (caseId: string, attachments?: CaseResult["attachments"]): CaseResult => ({
  caseId,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null,
  ...(attachments === undefined ? {} : { attachments })
})

const openRun = Effect.flatMap(Runs.Service, (runs) =>
  runs.open({ capture: "attached.pcap", lane: "fake", mode: "record", sessionDir: path.join(TREE, "attached"), identity: CURRENT }))

/** A run bundle of one cell `c1` at `runDir`, listing `lines` as its attachments. */
const writeBundle = (runDir: string, lines: ReadonlyArray<string>) => {
  fs.mkdirSync(path.join(runDir, "c1"), { recursive: true })
  fs.writeFileSync(
    path.join(runDir, "campaign.json"),
    JSON.stringify({ cells: [{ cell: { case: "case1", infra: "fake" }, passed: true, dir: "c1" }] })
  )
  fs.writeFileSync(path.join(runDir, "c1", "verdict.json"), JSON.stringify({ case: "case1", lane: "fake", status: "ok" }))
  fs.writeFileSync(path.join(runDir, "c1", Attachment.FILE), lines.join(""))
}

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
}, 60_000)

afterAll(async () => {
  fs.rmSync(TREE, { recursive: true, force: true })
  if (!Server.served) return
  await on("postgres", Server.dropDatabase(SCRATCH))
}, 60_000)

describe.skipIf(!Server.served)("attachments", () => {
  it("land with a cell read off its bundle, as it listed them, each a path under the cell's directory", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const store = yield* Attachments.Service
      const opened = yield* openRun
      writeBundle(opened.bundlePath, [
        Attachment.line({ ...LOG, title: "first" }),
        Attachment.line(RECORDS),
        Attachment.line(LOG),
        Attachment.line({ name: "escape", kind: "file", file: "../../etc/passwd", source: "replay" }),
        "not json\n"
      ])
      const reading = yield* Bundle.read(opened.bundlePath).pipe(Effect.provide(NodeServices.layer))
      yield* runs.finish({ id: opened.id, status: "done", cases: reading.cases })
      const held = yield* store.of({ scope: "cell", runId: opened.id, caseId: "case1" })
      expect(held.map(({ name, kind, title, file, path: at }) => ({ name, kind, title, file, at }))).toEqual([
        { name: "sut-log", kind: "log", title: "the SUT's log", file: "sut.log", at: path.join(opened.bundlePath, "c1", "sut.log") },
        {
          name: "call-records",
          kind: "records",
          title: undefined,
          file: "records.jsonl",
          at: path.join(opened.bundlePath, "c1", "records.jsonl")
        }
      ])
      expect(yield* store.byId(held[1]!.id)).toEqual(held[1])
    })))

  it("are replaced whole when the cell is written again, and kept where it states none", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const store = yield* Attachments.Service
      const opened = yield* openRun
      const dir = path.join(opened.bundlePath, "c1")
      const holder = { scope: "cell", runId: opened.id, caseId: "a" } as const
      yield* runs.finish({ id: opened.id, status: "done", cases: [cell("a", { dir, attachments: [LOG, RECORDS] })] })
      yield* runs.finish({ id: opened.id, status: "done", cases: [cell("a", { dir, attachments: [RECORDS] })] })
      expect((yield* store.of(holder)).map((one) => one.name)).toEqual(["call-records"])
      yield* runs.finish({ id: opened.id, status: "done", cases: [cell("a")] })
      expect((yield* store.of(holder)).map((one) => one.name)).toEqual(["call-records"])
      yield* runs.finish({ id: opened.id, status: "done", cases: [cell("a", { dir, attachments: [] })] })
      expect(yield* store.of(holder)).toEqual([])
    })))

  it("added late replace one of their name where it stands and follow the others", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const store = yield* Attachments.Service
      const opened = yield* openRun
      const dir = path.join(opened.bundlePath, "c1")
      const holder = { scope: "cell", runId: opened.id, caseId: "a" } as const
      yield* runs.finish({ id: opened.id, status: "done", cases: [cell("a", { dir, attachments: [LOG, RECORDS] })] })
      const late: Attachment.Attachment = { name: "routing-log", kind: "log", file: "routing.jsonl", source: "collector" }
      yield* store.add(holder, { dir, attachments: [late, { ...LOG, file: "sut-cut.log" }] })
      expect((yield* store.of(holder)).map((one) => [one.name, one.file])).toEqual([
        ["sut-log", "sut-cut.log"],
        ["call-records", "records.jsonl"],
        ["routing-log", "routing.jsonl"]
      ])
    })))

  it("are held by a batch and by a job apart from their cells, a link with no path", () =>
    withStore(Effect.gen(function*() {
      const store = yield* Attachments.Service
      const jobs = yield* Jobs.Service
      const batches = yield* Batches.Service
      const job = yield* jobs.create({ kind: "campaign", args: {}, createdBy: "test" })
      const batch = yield* batches.open({ jobId: job, lane: "fake", cells: ["a"] })
      const jobDir = path.join(TREE, "_jobs", String(job))
      const console: Attachment.Attachment = { name: "console", kind: "link", file: "http://lab.example/console", source: "pack" }
      yield* store.add({ scope: "batch", batchId: batch }, { dir: path.join(TREE, "batch"), attachments: [LOG] })
      yield* store.add({ scope: "job", jobId: job }, { dir: jobDir, attachments: [RECORDS, console] })
      expect((yield* store.of({ scope: "batch", batchId: batch })).map((one) => one.path)).toEqual([
        path.join(TREE, "batch", "sut.log")
      ])
      expect((yield* store.of({ scope: "job", jobId: job })).map((one) => [one.name, one.path])).toEqual([
        ["call-records", path.join(jobDir, "records.jsonl")],
        ["console", undefined]
      ])
      yield* batches.close(batch, null)
    })))

  it("go with their cells and batches in the result reset, a job's kept with the job", () =>
    withStore(Effect.gen(function*() {
      const runs = yield* Runs.Service
      const store = yield* Attachments.Service
      const jobs = yield* Jobs.Service
      const batches = yield* Batches.Service
      const reset = yield* ResultReset.Service
      const sql = yield* SqlClient
      const job = yield* jobs.create({ kind: "campaign", args: {}, createdBy: "test" })
      yield* jobs.close(job, { status: "done", exit: 0 })
      const batch = yield* batches.open({ jobId: job, lane: "fake", cells: ["a"] })
      yield* batches.close(batch, null)
      const jobDir = path.join(TREE, "_jobs", String(job))
      yield* store.add({ scope: "job", jobId: job }, { dir: jobDir, attachments: [LOG] })
      yield* store.add({ scope: "batch", batchId: batch }, { dir: path.join(TREE, "batch"), attachments: [LOG] })
      const opened = yield* openRun
      const dir = path.join(opened.bundlePath, "c1")
      yield* runs.finish({ id: opened.id, status: "done", cases: [cell("a", { dir, attachments: [LOG] })] })
      const jobsBefore = yield* sql<{ n: string }>`SELECT count(*) AS n FROM jobs`
      const kept = yield* store.of({ scope: "job", jobId: job })
      const plan = yield* reset.plan({})
      expect(plan.emptied).toContain("attachments")
      expect(plan.emptied).not.toContain("job_attachments")
      yield* reset.execute({})
      const left = yield* sql<{ n: string }>`SELECT count(*) AS n FROM attachments`
      expect(left[0]!.n).toBe("0")
      expect(yield* sql<{ n: string }>`SELECT count(*) AS n FROM jobs`).toEqual(jobsBefore)
      expect(yield* store.of({ scope: "job", jobId: job })).toEqual(kept)
      expect(kept.map((one) => one.path)).toEqual([path.join(jobDir, "sut.log")])
    })))
})
