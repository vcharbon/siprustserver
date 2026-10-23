/**
 * The refusal record on its own, against a real postgres: which standing cuts
 * have no refusal rows yet, and a record written after the cut it belongs to.
 * The suite builds and drops its OWN database on the test server (`./server.js`).
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Cuts from "../src/cuts.js"
import * as Refusals from "../src/refusals.js"

const SCRATCH = await Server.scratch("store_refusals_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withStore = <A, E>(
  effect: Effect.Effect<A, E, Refusals.Service | Cuts.Service | SqlClient>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Refusals.layer),
      Effect.provide(Cuts.layer),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

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
  await on(
    "postgres",
    Effect.gen(function*() {
      yield* Server.dropDatabase(SCRATCH)
    })
  )
}, 60_000)

describe.skipIf(!served)("Refusals", () => {
  it("names the standing cuts with their row count, and records one after the fact", () =>
    withStore(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const refusals = yield* Refusals.Service
      const older = yield* cuts.open()
      yield* cuts.record({ prepId: older, capture: "capture_bare.pcap.gz", flows: null, cases: [], refusals: [] })
      const newer = yield* cuts.open()
      yield* cuts.record({ prepId: newer, capture: "capture_bare.pcap.gz", flows: null, cases: [], refusals: [] })
      yield* cuts.record({
        prepId: newer,
        capture: "capture_told.pcap.gz",
        flows: null,
        cases: [],
        refusals: [{ caseId: "capture_told.pcap.gz-cut1", reasons: ["source-call-incomplete"], callIds: [], detail: "x" }]
      })

      // The STANDING cut only: the older reading of the bare capture is nobody's question.
      expect(yield* refusals.standing).toEqual([
        { capture: "capture_bare.pcap.gz", prepId: newer, rows: 0 },
        { capture: "capture_told.pcap.gz", prepId: newer, rows: 1 }
      ])

      const written = yield* refusals.record(newer, "capture_bare.pcap.gz", [
        { caseId: "capture_bare.pcap.gz-source", reasons: ["source-truncated-at-edge"], callIds: [], detail: "short" }
      ])
      expect(written).toEqual(1)
      expect((yield* refusals.standing).map((one) => one.rows)).toEqual([1, 1])
    })))

  it("answers rows written, one per call, whatever the records it was handed", () =>
    withStore(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const refusals = yield* Refusals.Service
      const prep = yield* cuts.open()
      yield* cuts.record({ prepId: prep, capture: "capture_counted.pcap.gz", flows: null, cases: [], refusals: [] })
      const written = yield* refusals.record(prep, "capture_counted.pcap.gz", [
        { caseId: "capture_counted.pcap.gz-auto", reasons: ["a"], callIds: [], detail: "one" },
        { caseId: "capture_counted.pcap.gz-auto", reasons: ["b"], callIds: [], detail: "two" },
        { caseId: "capture_counted.pcap.gz-cut2", reasons: ["a"], callIds: [], detail: "three" }
      ])
      expect(written).toEqual(2)
    })))

  it("never offers a BROKEN standing cut, so a stale record is not attributed to it", () =>
    withStore(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const refusals = yield* Refusals.Service
      const capture = "capture_then_broke.pcap.gz"
      const older = yield* cuts.open()
      yield* cuts.record({
        prepId: older,
        capture,
        flows: null,
        cases: [],
        refusals: [{ caseId: `${capture}-auto`, reasons: ["source-call-incomplete"], callIds: [], detail: "x" }]
      })
      const newer = yield* cuts.open()
      yield* cuts.record({ prepId: newer, capture, flows: null, broke: "[prep] enrich failed", cases: [], refusals: [] })
      // Neither the broken standing cut nor the older reading behind it.
      expect((yield* refusals.standing).filter((one) => one.capture === capture)).toEqual([])
      expect(yield* refusals.ofCase(capture, `${capture}-auto`)).toBeNull()
    })))

  it("answers one call's refusal under the standing cut, merged over its reasons", () =>
    withStore(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const refusals = yield* Refusals.Service
      const capture = "capture_one_call.pcap.gz"
      const older = yield* cuts.open()
      yield* cuts.record({
        prepId: older,
        capture,
        flows: null,
        cases: [],
        refusals: [{ caseId: `${capture}-auto`, reasons: ["gone-since"], callIds: ["old"], detail: "older reading" }]
      })
      const newer = yield* cuts.open()
      yield* cuts.record({
        prepId: newer,
        capture,
        flows: null,
        cases: [],
        refusals: [
          { caseId: `${capture}-auto`, reasons: ["source-call-incomplete"], callIds: ["c1"], detail: "no final" },
          { caseId: `${capture}-auto`, reasons: ["egress-answer-not-captured"], callIds: ["c1", "c2"], detail: "no 200" },
          { caseId: `${capture}-cut2`, reasons: ["source-truncated-at-edge"], callIds: ["c3"], detail: "edge" }
        ]
      })
      // The standing cut's record, both reasons, never the older reading's.
      expect(yield* refusals.ofCase(capture, `${capture}-auto`)).toEqual({
        caseId: `${capture}-auto`,
        reasons: ["source-call-incomplete", "egress-answer-not-captured"],
        callIds: ["c1", "c2"],
        detail: "no final; no 200"
      })
      // A call the cut did not refuse, and a capture it never read.
      expect(yield* refusals.ofCase(capture, `${capture}-case1`)).toBeNull()
      expect(yield* refusals.ofCase("capture_unknown.pcap.gz", "capture_unknown.pcap.gz-auto")).toBeNull()
    })))
})
