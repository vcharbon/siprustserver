/**
 * Every cited form of a case id, read back to the call against a real
 * postgres, and the one-line reason a case id names no case of
 * a pinned cut. The suite builds and drops its OWN database and skips where
 * no server answers.
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Cuts from "../src/cuts.js"
import { type CaptureCutRequest, type CutCaseRequest, type Refusal, wholeFlows } from "../src/model.js"
import * as Refusals from "../src/refusals.js"
import * as Resolver from "../src/resolver.js"
import * as Runs from "../src/runs.js"
import * as Sessions from "../src/sessions.js"
import { CURRENT } from "./identity.js"

const SCRATCH = await Server.scratch("store_resolver_test")

/** A deployment naming its captures `trace_…`, its stems included. */
const GRAMMAR: Resolver.Grammar = {
  isCapture: (input) => Resolver.DEFAULT_GRAMMAR.isCapture(input) || input.startsWith("trace_")
}

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

type Services = Cuts.Service | Runs.Service | Refusals.Service | Resolver.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([Cuts.layer, Runs.layer, Refusals.layer, Resolver.layerWith(GRAMMAR)]),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

const caseOf = (id: string, callKey: string): CutCaseRequest => ({
  caseId: id,
  callKey,
  document: { id, actors: [], steps: [] },
  assets: [],
  ledger: [],
  egressEndpoint: null
})

const cutOf = (
  prepId: number,
  capture: string,
  cases: ReadonlyArray<CutCaseRequest>,
  refusals: ReadonlyArray<Refusal> = []
): CaptureCutRequest => ({
  prepId,
  capture,
  flows: wholeFlows(bytes(`{"capture":"${capture}"}`)),
  cases,
  refusals
})

const BULK = "trace_bulk.pcapng"
const UUID = "trace_ffeb8bb4-163b-41f4-ac6c-a2334f33f8f7.pcap.gz"
const AUTO = "trace_204071.pcap.gz"
const OTHER = "trace_ca510222-0000-4000-8000-000000000001.pcap.gz"

/** The store the suite reads: two preps, a refusal, a re-cut that dropped a case, and one run. */
const seed = Effect.gen(function*() {
  const cuts = yield* Cuts.Service
  const runs = yield* Runs.Service
  const first = yield* cuts.open()
  yield* cuts.record(cutOf(first, BULK, [
    caseOf(`${BULK}-case1217`, "1-key1217@10.0.0.1"),
    caseOf(`${BULK}-case1`, "1-key1@10.0.0.1"),
    caseOf(`${BULK}-case5`, "1-key5@10.0.0.1")
  ]))
  yield* cuts.record(cutOf(first, UUID, [caseOf(`${UUID}-case1`, "uuid-call@host")], [
    { caseId: `${UUID}-legs3+4`, reasons: ["source-no-dialog-at-boundary"], callIds: ["refused-call@host"], detail: "no dialog" }
  ]))
  yield* cuts.record(cutOf(first, AUTO, [caseOf(`${AUTO}-auto`, "auto-call@host")]))
  yield* cuts.record(cutOf(first, OTHER, [caseOf(`${OTHER}-auto`, "other-call@host")]))
  yield* cuts.finish(first)
  const second = yield* cuts.open()
  // The re-cut keeps case1217 and case1 and no longer names case5.
  yield* cuts.record(cutOf(second, BULK, [
    caseOf(`${BULK}-case1217`, "1-key1217@10.0.0.1"),
    caseOf(`${BULK}-case1`, "1-key1@10.0.0.1")
  ]))
  yield* cuts.finish(second)
  const run = yield* runs.open({
    capture: UUID,
    lane: "fake",
    mode: "record",
    sessionDir: "/w/uuid",
    prepId: first,
    identity: CURRENT
  })
  yield* runs.finish({
    id: run.id,
    status: "done",
    cases: [{
      caseId: `${UUID}-case1`,
      passed: false,
      verdict: "failed",
      skipped: null,
      failedStep: "s9",
      rfcGating: 0,
      mainError: null,
      classification: null,
      document: null
    }]
  })
  // A run of BULK pinned to the first prep: what it replayed is that prep's reading.
  const pinned = yield* runs.open({
    capture: BULK,
    lane: "fake",
    mode: "record",
    sessionDir: "/w/bulk",
    prepId: first,
    identity: CURRENT
  })
  yield* runs.finish({ id: pinned.id, status: "done", cases: [] })
  return { first, second, run: run.id, pinned: pinned.id }
})

let store:
  | { readonly first: number; readonly second: number; readonly run: number; readonly pinned: number }
  | undefined

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
  store = await withStore(seed)
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

const resolve = (input: string, options?: Resolver.Options) =>
  withStore(Effect.flatMap(Resolver.Service, (resolver) => resolver.resolve(input, options)))

describe("Resolver.formOf", () => {
  it("reads each cited form", () => {
    expect([
      "run1051725/case1",
      "run1051725",
      "case1",
      "-case4905",
      "-auto",
      "trace_ca510222-…-auto",
      `${BULK}-case1217`,
      `${UUID}-legs3+4`,
      "trace_204285",
      "1-key1@10.0.0.1"
    ].map((input) => Resolver.formOf(input, GRAMMAR))).toEqual([
      "run-case",
      "run",
      "bare-case",
      "bare-case",
      "bare-case",
      "pattern",
      "case-id",
      "case-id",
      "capture",
      "call-id"
    ])
  })

  it("reads a capture by its file name alone where the deployment states no grammar", () => {
    expect(["trace_204285.pcapng", "trace_204285", "SDabc-1@host"].map((input) => Resolver.formOf(input)))
      .toEqual(["capture", "call-id", "call-id"])
  })
})

describe.skipIf(!served)("Resolver.resolve", () => {
  it("resolves a full case id to its call at the newest prep naming it", async () => {
    const answer = await resolve(`${BULK}-case1217`)
    expect(answer.status).toBe("resolved")
    expect(answer.calls).toEqual([{
      capture: BULK,
      caseId: `${BULK}-case1217`,
      callKey: "1-key1217@10.0.0.1",
      prepId: store!.second,
      kind: "case",
      reasons: [],
      standing: true
    }])
  })

  it("resolves a case id that names its capture by its stem", async () => {
    const answer = await resolve("trace_204071-auto")
    expect(answer.status).toBe("resolved")
    expect(answer.calls[0]?.caseId).toBe(`${AUTO}-auto`)
  })

  it("says a case the standing cut no longer names was last named by an older prep", async () => {
    const answer = await resolve(`${BULK}-case5`)
    expect(answer.status).toBe("resolved")
    expect(answer.calls[0]?.standing).toBe(false)
    expect(answer.note).toContain(`last named by prep${store!.first}`)
  })

  it("answers a refused call with its reasons", async () => {
    const answer = await resolve(`${UUID}-legs3+4`)
    expect(answer.status).toBe("refused")
    expect(answer.calls[0]?.reasons).toEqual(["source-no-dialog-at-boundary"])
  })

  it("lists the candidates of a bare caseN", async () => {
    const answer = await resolve("case1")
    expect(answer.status).toBe("ambiguous")
    expect(answer.total).toBe(2)
    expect(answer.calls.map((call) => call.caseId)).toEqual([`${BULK}-case1`, `${UUID}-case1`])
  })

  it("resolves a bare suffix only one capture holds", async () => {
    const answer = await resolve("-case1217")
    expect(answer.status).toBe("resolved")
    expect(answer.calls[0]?.capture).toBe(BULK)
  })

  it("resolves an elided id to the one call it can stand for", async () => {
    const answer = await resolve("trace_ca510222-…-auto")
    expect(answer.status).toBe("resolved")
    expect(answer.calls[0]?.caseId).toBe(`${OTHER}-auto`)
  })

  it("resolves runN/caseN through the run's capture", async () => {
    const answer = await resolve(`run${store!.run}/case1`)
    expect(answer.status).toBe("resolved")
    expect(answer.calls[0]?.caseId).toBe(`${UUID}-case1`)
  })

  it("resolves runN/caseN through the cut run N pinned, and says when and what the run was", async () => {
    const answer = await resolve(`run${store!.pinned}/case1`)
    expect(answer.status).toBe("resolved")
    expect(answer.calls[0]?.prepId).toBe(store!.first)
    expect(answer.note).toMatch(new RegExp(`run ${store!.pinned} \\(started \\d{4}-\\d{2}-\\d{2}T[^)]*, ${BULK}, prep${store!.first}\\)`))
  })

  it("names a run as a run, and a run above the store's highest as pre-rebuild", async () => {
    expect((await resolve(`run${store!.run}`)).status).toBe("a-run")
    const old = await resolve("run1051725/case1")
    expect(old.status).toBe("pre-rebuild")
    expect(old.note).toContain("before the rebuild")
  })

  it("answers an id cited before the store's rebuild as pre-rebuild", async () => {
    const answer = await resolve(`${BULK}-case1217`, { citedOn: "2000-01-01" })
    expect(answer.status).toBe("pre-rebuild")
  })

  it("names a capture as a capture, by its name or its stem", async () => {
    const answer = await resolve("trace_204071")
    expect(answer.status).toBe("a-capture")
    expect(answer.note).toContain(`${AUTO} is a capture`)
  })

  it("resolves a Call-ID to the case keyed by it, and to the refusal naming it", async () => {
    expect((await resolve("auto-call@host")).calls.map((call) => call.caseId)).toEqual([`${AUTO}-auto`])
    const refused = await resolve("refused-call@host")
    expect(refused.status).toBe("refused")
    expect(refused.calls[0]?.caseId).toBe(`${UUID}-legs3+4`)
  })

  it("says what an id nothing holds is, naming the accepted forms", async () => {
    const answer = await resolve("nothing-at-all")
    expect(answer.status).toBe("not-found")
    expect(answer.note).toContain(Resolver.FORMS)
  })

  it("names the accepted forms where a case id or a bare suffix names nothing", async () => {
    for (const input of [`${BULK}-case99999`, "case99999", "trace_nowhere-…-auto"]) {
      const answer = await resolve(input)
      expect(answer.status).toBe("not-found")
      expect(answer.note).toContain(Resolver.FORMS)
    }
  })
})

describe.skipIf(!served)("Refusals.whyNot", () => {
  const whyNot = (capture: string, caseId: string, prepId: number) =>
    withStore(Effect.flatMap(Refusals.Service, (refusals) => refusals.whyNot(capture, caseId, prepId)))

  it("says the pinned cut refused it, with the reasons", async () => {
    const absence = await whyNot(UUID, `${UUID}-legs3+4`, store!.second)
    expect(absence.token).toBe("refused")
    expect(absence.reason).toContain("source-no-dialog-at-boundary")
  })

  it("says another prep named it where the pinned cut does not", async () => {
    const absence = await whyNot(BULK, `${BULK}-case5`, store!.second)
    expect(absence.token).toBe("not-in-this-cut")
    expect(absence.reason).toContain(`prep${store!.first} named it`)
  })

  it("says no cut ever named an id that is in none", async () => {
    expect((await whyNot(BULK, `${BULK}-case99`, store!.second)).token).toBe("unknown-id")
  })

  it("says no cut of the capture stands as of a prep before its first", async () => {
    expect((await whyNot(BULK, `${BULK}-case1`, 0)).token).toBe("no-cut")
  })
})
