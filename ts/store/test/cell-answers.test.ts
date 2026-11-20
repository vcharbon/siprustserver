/**
 * A landed cell answered later, against a real postgres: a collector's lines
 * merge into the cell's (one in place of the pending line of its name and
 * source, any other after, an answered line never overwritten), its
 * attachments join the cell's, and the cell's final status is read off the
 * merged lines. The suite builds and drops its own database.
 */
import type { Attachment } from "@sip/contracts"
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Attachments from "../src/attachments.js"
import * as CellAnswers from "../src/cell-answers.js"
import * as Client from "../src/client.js"
import type { CaseResult } from "../src/model.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import * as Verifications from "../src/verifications.js"
import { CURRENT } from "./identity.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_cell_answers_test")

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

type Services = Attachments.Service | CellAnswers.Service | Runs.Service | Verifications.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([Attachments.layer, CellAnswers.layer, Runs.layer, Verifications.layer]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const CALL: Verifications.Line = { name: "call", source: "interpreter", answer: "pass" }
const AUDIT: Verifications.Line = { name: "rfc-audit", source: "rfc audit", answer: "pass" }
const RECORDS: Attachment.Attachment = { name: "call-records", kind: "records", file: "records.jsonl", source: "replay" }
const LOG: Attachment.Attachment = { name: "lab-log", kind: "log", title: "the lab's log", file: "collected/lab-log/a.log", source: "lab-log" }

const cell = (caseId: string, verifications: ReadonlyArray<Verifications.Line>): CaseResult => ({
  caseId,
  passed: true,
  verdict: "ok",
  skipped: null,
  failedStep: null,
  rfcGating: null,
  mainError: null,
  classification: null,
  document: null,
  verifications,
  attachments: { dir: `/w/answers/${caseId}`, attachments: [RECORDS] }
})

/** A run whose one cell `a` landed with `verifications` and one attachment; its id. */
const landedWith = (verifications: ReadonlyArray<Verifications.Line>) =>
  Effect.gen(function*() {
    const runs = yield* Runs.Service
    const opened = yield* runs.open({ capture: "answers.pcap", lane: "fake", mode: "record", sessionDir: "/w/answers", identity: CURRENT })
    yield* runs.finish({ id: opened.id, status: "done", cases: [cell("a", verifications)] })
    return opened.id
  })

/** A run whose one cell `a` landed passing with two lines and one attachment; its id. */
const landed = landedWith([CALL, AUDIT])

const statusOf = (runId: number) =>
  Effect.flatMap(SqlClient, (sql) =>
    Effect.map(
      sql<{ status: string }>`SELECT status FROM cell_statuses WHERE run_id = ${runId} AND case_id = 'a'`,
      (rows) => rows[0]?.status
    ))

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
  if (!Server.served) return
  await on("postgres", Server.dropDatabase(SCRATCH))
}, 60_000)

describe.skipIf(!Server.served)("a landed cell answered later", () => {
  it("holds a collector's not-checkable line after its own, its attachment after the cell's, and reads unverified", () =>
    withStore(Effect.gen(function*() {
      const answers = yield* CellAnswers.Service
      const lines = yield* Verifications.Service
      const attachments = yield* Attachments.Service
      const runId = yield* landed
      const unanswered: Verifications.Line = { name: "lab-log", source: "lab-log", answer: "not-checkable", message: "collect exited 1" }
      yield* answers.merge(runId, "a", { verifications: [unanswered], held: { dir: "/w/answers/a", attachments: [LOG] } })
      expect(yield* lines.of({ scope: "cell", runId, caseId: "a" })).toEqual([CALL, AUDIT, unanswered])
      expect((yield* attachments.of({ scope: "cell", runId, caseId: "a" })).map((one) => [one.name, one.path])).toEqual([
        ["call-records", "/w/answers/a/records.jsonl"],
        ["lab-log", "/w/answers/a/collected/lab-log/a.log"]
      ])
      expect(yield* statusOf(runId)).toBe("unverified")
    })))

  it("replaces the pending line of the same name and source where it stands, and the cell's status follows it", () =>
    withStore(Effect.gen(function*() {
      const answers = yield* CellAnswers.Service
      const lines = yield* Verifications.Service
      const waiting: Verifications.Line = { name: "lab-log", source: "lab-log", answer: "pending" }
      const runId = yield* landedWith([CALL, waiting, AUDIT])
      const failed: Verifications.Line = { name: "lab-log", source: "lab-log", answer: "fail", message: "a late finding" }
      const other: Verifications.Line = { name: "call", source: "lab-log", answer: "pass" }
      yield* answers.merge(runId, "a", { verifications: [failed, other], held: { dir: "/w/answers/a", attachments: [] } })
      expect(yield* lines.of({ scope: "cell", runId, caseId: "a" })).toEqual([CALL, failed, AUDIT, other])
      expect(yield* statusOf(runId)).toBe("fail")
    })))

  it("never overwrites a line already answered: a failed line of the same name and source stands, the cell still failed", () =>
    withStore(Effect.gen(function*() {
      const answers = yield* CellAnswers.Service
      const lines = yield* Verifications.Service
      const failedCall: Verifications.Line = { name: "call", source: "interpreter", answer: "fail", message: "no 200 OK" }
      const runId = yield* landedWith([failedCall, AUDIT])
      const passing: Verifications.Line = { name: "call", source: "interpreter", answer: "pass" }
      yield* answers.merge(runId, "a", { verifications: [passing], held: { dir: "/w/answers/a", attachments: [] } })
      expect(yield* lines.of({ scope: "cell", runId, caseId: "a" })).toEqual([failedCall, AUDIT, passing])
      expect(yield* statusOf(runId)).toBe("fail")
    })))
})
