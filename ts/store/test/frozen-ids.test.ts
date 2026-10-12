/**
 * Frozen case ids and the case maps against a real postgres: a recorded cut
 * freezes every id it names, a re-cut naming a frozen call or id otherwise is
 * refused whole, a seed keeps what stands, and a reading's case map — with
 * where each call starts — outlives the reading's own cut rows. The suite
 * builds and drops its own database.
 */
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as CaseIds from "../src/case-ids.js"
import * as CaseMaps from "../src/case-maps.js"
import * as Client from "../src/client.js"
import * as Cuts from "../src/cuts.js"
import type { CaptureCutRequest, CutCaseRequest, FlowsSource, Refusal } from "../src/model.js"
import * as Server from "./server.js"

const SCRATCH = await Server.scratch("store_frozen_ids_test")

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(Server.urlFor(database)))))

type Services = Cuts.Service | CaseIds.Service | CaseMaps.Service | SqlClient

const withStore = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide([Cuts.layer, CaseIds.layer, CaseMaps.layer]),
      Effect.provide(Client.layerFromUrl(Server.urlFor(SCRATCH)))
    )
  )

const exitOf = <A, E>(effect: Effect.Effect<A, E, Services>): Promise<Exit.Exit<A, E>> =>
  withStore(Effect.exit(effect))

const served = Server.served

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
  await on("postgres", Server.dropDatabase(SCRATCH))
}, 60_000)

const text = (value: string): Uint8Array => new TextEncoder().encode(value)

/** A flows document of these legs, each a Call-ID whose first message is at `ts_us` from `src` to `dst`. */
const flowsOf = (
  capture: string,
  legs: ReadonlyArray<{ callId: string; ts_us: number; src: string; dst: string }>
): FlowsSource => () => [
  { kind: "head", bytes: text(`{"capture":"${capture}","legs":[\n`) },
  ...legs.map((leg, at) => ({
    kind: "leg" as const,
    bytes: text(
      JSON.stringify({ call_id: leg.callId, msgs: [{ ts_us: leg.ts_us, src: leg.src, dst: leg.dst }] }) +
        (at === legs.length - 1 ? "\n" : ",\n")
    )
  })),
  { kind: "tail", bytes: text("]}\n") }
]

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
  refusals: ReadonlyArray<Refusal> = [],
  flows: FlowsSource | null = null
): CaptureCutRequest => ({ prepId, capture, flows, cases, refusals })

const recordAt = (request: (prep: number) => CaptureCutRequest) =>
  Effect.gen(function*() {
    const cuts = yield* Cuts.Service
    const prep = yield* cuts.open()
    yield* cuts.record(request(prep))
    yield* cuts.finish(prep)
    return prep
  })

/** The frozen ids of `capture`, call key → case id. */
const frozenOf = (capture: string): Promise<Record<string, string>> =>
  withStore(Effect.flatMap(CaseIds.Service, (ids) => ids.of([capture]))).then((frozen) =>
    Object.fromEntries((frozen.get(capture) ?? []).map((entry) => [entry.callKey, entry.caseId]))
  )

describe.skipIf(!served)("frozen case ids", () => {
  const CAPTURE = "frozen.pcap"

  it("freezes every id a recorded cut names, its keyed refusals included", async () => {
    await withStore(recordAt((prep) =>
      cutOf(prep, CAPTURE, [caseOf(`${CAPTURE}-a`, "call-a@h"), caseOf(`${CAPTURE}-b`, "call-b@h")], [
        { caseId: `${CAPTURE}-r`, reasons: ["refused-why"], callIds: ["call-r@h"], detail: "", callKey: "call-r@h" },
        { caseId: `${CAPTURE}-x`, reasons: ["refused-why"], callIds: ["call-x@h"], detail: "" }
      ])
    ))
    expect(await frozenOf(CAPTURE)).toEqual({
      "call-a@h": `${CAPTURE}-a`,
      "call-b@h": `${CAPTURE}-b`,
      "call-r@h": `${CAPTURE}-r`
    })
  })

  it("refuses a re-cut naming a frozen call by another id, and the capture stands at its previous reading", async () => {
    const before = await withStore(Effect.flatMap(Cuts.Service, (cuts) => cuts.standing(CAPTURE)))
    const exit = await exitOf(recordAt((prep) =>
      cutOf(prep, CAPTURE, [caseOf(`${CAPTURE}-a2`, "call-a@h"), caseOf(`${CAPTURE}-b`, "call-b@h")])
    ))
    expect(Exit.isFailure(exit)).toBe(true)
    expect(String(Exit.isFailure(exit) ? exit.cause : "")).toContain("clash with the frozen ids")
    const after = await withStore(Effect.flatMap(Cuts.Service, (cuts) => cuts.standing(CAPTURE)))
    expect(after?.prepId).toBe(before?.prepId)
  })

  it("refuses a re-cut naming a frozen id for another call", async () => {
    const exit = await exitOf(recordAt((prep) =>
      cutOf(prep, CAPTURE, [caseOf(`${CAPTURE}-a`, "call-other@h")])
    ))
    expect(Exit.isFailure(exit)).toBe(true)
  })

  it("keeps a re-cut naming every frozen call by its frozen id, and freezes the new call beside them", async () => {
    await withStore(recordAt((prep) =>
      cutOf(prep, CAPTURE, [caseOf(`${CAPTURE}-a`, "call-a@h"), caseOf(`${CAPTURE}-c`, "call-c@h")])
    ))
    const frozen = await frozenOf(CAPTURE)
    expect(frozen["call-c@h"]).toBe(`${CAPTURE}-c`)
    expect(frozen["call-b@h"]).toBe(`${CAPTURE}-b`)
  })

  it("re-keys a frozen id only where the reading names the key it replaces", async () => {
    const without = await exitOf(recordAt((prep) =>
      cutOf(prep, CAPTURE, [caseOf(`${CAPTURE}-c`, "call-c@h#ingress@7")])
    ))
    expect(Exit.isFailure(without)).toBe(true)
    await withStore(recordAt((prep) =>
      cutOf(prep, CAPTURE, [{ ...caseOf(`${CAPTURE}-c`, "call-c@h#ingress@7"), replaces: "call-c@h" }])
    ))
    const frozen = await frozenOf(CAPTURE)
    expect(frozen["call-c@h#ingress@7"]).toBe(`${CAPTURE}-c`)
    expect(frozen["call-c@h"]).toBeUndefined()
  })

  it("seeds what does not clash and counts the rest, whatever another capture holds", async () => {
    const seeded = await withStore(Effect.flatMap(CaseIds.Service, (ids) =>
      ids.freeze(CAPTURE, [
        { caseId: `${CAPTURE}-a`, callKey: "call-a@h" },
        { caseId: `${CAPTURE}-old-a`, callKey: "call-a@h" },
        { caseId: `${CAPTURE}-b`, callKey: "call-moved@h" },
        { caseId: `${CAPTURE}-d`, callKey: "call-d@h" }
      ], "seed", "seed")
    ))
    expect(seeded).toEqual({ added: 1, held: 1, skipped: 2, rekeyed: 0 })
    const elsewhere = await withStore(Effect.flatMap(CaseIds.Service, (ids) =>
      ids.freeze("another.pcap", [{ caseId: `${CAPTURE}-a`, callKey: "call-a@h" }], "seed")
    ))
    expect(elsewhere.added).toBe(1)
  })
})

describe.skipIf(!served)("a store whose readings predate the frozen ids", () => {
  const LEGACY = "legacy.pcap"

  /** Two readings written as a store without frozen ids holds them: cut rows alone. */
  const legacy = Effect.gen(function*() {
    const sql = yield* SqlClient
    const cuts = yield* Cuts.Service
    const older = yield* cuts.open()
    const newer = yield* cuts.open()
    yield* sql`INSERT INTO case_documents (hash, document) VALUES ('legacy-doc', '{}') ON CONFLICT DO NOTHING`
    yield* sql`INSERT INTO cases (hash, document_hash) VALUES ('legacy-case', 'legacy-doc') ON CONFLICT DO NOTHING`
    for (const prep of [older, newer]) {
      yield* sql`INSERT INTO capture_cuts (prep_id, capture) VALUES (${prep}, ${LEGACY})`
    }
    // The older reading named call-1 `-case1`; the newer renamed it `-cut9` and added call-2.
    yield* sql`
      INSERT INTO cut_cases (prep_id, capture, case_id, call_key, case_hash) VALUES
        (${older}, ${LEGACY}, ${`${LEGACY}-case1`}, 'call-1@h', 'legacy-case'),
        (${older}, ${LEGACY}, ${`${LEGACY}-case2`}, 'call-old@h', 'legacy-case'),
        (${newer}, ${LEGACY}, ${`${LEGACY}-cut9`}, 'call-1@h', 'legacy-case'),
        (${newer}, ${LEGACY}, ${`${LEGACY}-case2`}, 'call-2@h', 'legacy-case')
    `
    return { older, newer }
  })

  it("freezes every keyed id newest reading first, and refuses a new reading until the capture is seeded", async () => {
    await withStore(legacy)
    await withStore(Effect.gen(function*() {
      yield* CaseMaps.exportReadings
      yield* CaseIds.seedFromReadings
    }))
    expect(await frozenOf(LEGACY)).toEqual({ "call-1@h": `${LEGACY}-cut9`, "call-2@h": `${LEGACY}-case2` })
    const early = await exitOf(recordAt((prep) => cutOf(prep, LEGACY, [caseOf(`${LEGACY}-case2`, "call-2@h")])))
    expect(String(Exit.isFailure(early) ? early.cause : "")).toContain("not frozen yet")
    await withStore(Effect.flatMap(CaseIds.Service, (ids) => ids.markSeeded([LEGACY])))
    await withStore(recordAt((prep) => cutOf(prep, LEGACY, [caseOf(`${LEGACY}-case2`, "call-2@h")])))
  })

  it("lets the first reading of a capture never read before through, and counts it seeded", async () => {
    await withStore(recordAt((prep) => cutOf(prep, "fresh.pcap", [caseOf("fresh.pcap-a", "call-fa@h")])))
    expect(await withStore(Effect.flatMap(CaseIds.Service, (ids) => ids.unseeded(["fresh.pcap", LEGACY]))))
      .toEqual([])
  })
})

describe.skipIf(!served)("case maps", () => {
  const CAPTURE = "maps.pcap"

  it("keeps a reading's ids, and where each call starts, after the reading's cut rows go", async () => {
    const prep = await withStore(recordAt((p) =>
      cutOf(
        p,
        CAPTURE,
        [caseOf(`${CAPTURE}-a`, "call-a@h#ingress1")],
        [{ caseId: `${CAPTURE}-r`, reasons: ["refused-why"], callIds: ["1-call-r@h", "call-r@h"], detail: "" }],
        flowsOf(CAPTURE, [
          { callId: "call-a@h", ts_us: 1_789_646_119_005_979, src: "10.0.0.9:5060", dst: "10.0.0.1:5060" },
          { callId: "call-r@h", ts_us: 1_789_646_120_000_000, src: "10.0.0.8:5060", dst: "10.0.0.1:5060" }
        ])
      )
    ))
    const located = await withStore(Effect.flatMap(CaseMaps.Service, (maps) => maps.locate))
    expect(located).toBe(2)
    await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* sql`DELETE FROM preps WHERE id = ${prep}`
    }))
    const rows = await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      return yield* sql<{ case_id: string; kind: string; call_key: string | null; src: string; first_at: Date }>`
        SELECT case_id, kind, call_key, src, first_at FROM case_maps WHERE prep_id = ${prep} ORDER BY case_id
      `
    }))
    expect(rows.map((row) => [row.case_id, row.kind, row.call_key, row.src])).toEqual([
      [`${CAPTURE}-a`, "case", "call-a@h#ingress1", "10.0.0.9:5060"],
      [`${CAPTURE}-r`, "refused", null, "10.0.0.8:5060"]
    ])
    expect(rows[0]!.first_at.getTime()).toBe(1_789_646_119_005)
  })

  it("keeps where each call starts where the cut stated it, and hands it back with the frozen id", async () => {
    const start = { at_us: 1_789_646_119_005_979, src: "10.0.0.7:5060", dst: "10.0.0.1:5060" }
    await withStore(recordAt((p) => cutOf(p, "stated.pcap", [{ ...caseOf("stated.pcap-a", "call-st@h"), start }])))
    const frozen = await withStore(Effect.flatMap(CaseIds.Service, (ids) => ids.of(["stated.pcap"])))
    expect(frozen.get("stated.pcap")).toEqual([{ caseId: "stated.pcap-a", callKey: "call-st@h", first: start }])
  })

  it("surveys the documents of each capture's standing cut alone, in current_documents", async () => {
    await withStore(recordAt((p) => cutOf(p, "survey.pcap", [caseOf("survey.pcap-a", "call-sa@h")])))
    await withStore(recordAt((p) => cutOf(p, "survey.pcap", [caseOf("survey.pcap-b", "call-sb@h")])))
    const rows = await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      return yield* sql<{ case_id: string; call_key: string; id: string }>`
        SELECT case_id, call_key, document->>'id' AS id FROM current_documents WHERE capture = 'survey.pcap'
      `
    }))
    expect(rows).toEqual([{ case_id: "survey.pcap-b", call_key: "call-sb@h", id: "survey.pcap-b" }])
  })

  it("exports a reading recorded before its map was written, once", async () => {
    const prep = await withStore(recordAt((p) => cutOf(p, CAPTURE, [caseOf(`${CAPTURE}-e`, "call-e@h")])))
    await withStore(Effect.gen(function*() {
      const sql = yield* SqlClient
      yield* sql`DELETE FROM case_maps WHERE prep_id = ${prep}`
    }))
    expect(await withStore(Effect.flatMap(CaseMaps.Service, (maps) => maps.unexported))).toBe(1)
    const first = await withStore(Effect.flatMap(CaseMaps.Service, (maps) => maps.export))
    expect(first).toEqual({ cases: 1, refusals: 0 })
    expect(await withStore(Effect.flatMap(CaseMaps.Service, (maps) => maps.export))).toEqual({ cases: 0, refusals: 0 })
    expect(await withStore(Effect.flatMap(CaseMaps.Service, (maps) => maps.unexported))).toBe(0)
  })
})
