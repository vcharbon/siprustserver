/**
 * The population record and the cut fingerprint, against a real postgres.
 *
 * The load-bearing claims are the ones a fake cannot make: a second add of a
 * held capture counting as nothing new, a seed refusing a table that already
 * holds rows, and the newest cut of a capture answering with the code that took
 * it. The suite builds and drops its OWN database, so the server's other
 * databases are never touched (`./server.js`).
 */
import * as Effect from "effect/Effect"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as Cuts from "../src/cuts.js"
import * as Population from "../src/population.js"

const SCRATCH = await Server.scratch("store_population_test")

const urlFor = Server.urlFor

const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withStore = <A, E>(
  effect: Effect.Effect<A, E, Population.Service | Cuts.Service | SqlClient>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Population.layer),
      Effect.provide(Cuts.layer),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

const empty = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`DELETE FROM population`
})

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

describe.skipIf(!served)("Population", () => {
  it("counts only the captures it had never held", () =>
    withStore(Effect.gen(function*() {
      yield* empty
      const population = yield* Population.Service
      const one = "capture_one.pcap.gz"
      const two = "capture_two.pcap.gz"

      expect(yield* population.add([
        { capture: one, sourcePath: "/pcaps/one.pcap.gz", ingested: true },
        { capture: two, sourcePath: "/pcaps/two.pcap.gz" }
      ])).toBe(2)
      expect(yield* population.add([{ capture: one, sourcePath: "/elsewhere/one.pcap.gz" }])).toBe(0)

      const held = yield* population.list
      expect(held.map((member) => member.capture)).toEqual([one, two])
      // A re-add re-points the row at the pcap the host just read.
      expect(held[0]!.sourcePath).toBe("/elsewhere/one.pcap.gz")
      // … and keeps what it already knew: an add that ingested nothing does
      // not un-ingest a capture the store holds artifacts for.
      expect(held[0]!.ingestedAt).not.toBeNull()
      expect(held[1]!.ingestedAt).toBeNull()

      yield* population.ingested(two)
      expect((yield* population.list)[1]!.ingestedAt).not.toBeNull()
    })))

  it("moves the extractor facts only when the adder states them, the ingest instant only on an extraction", () =>
    withStore(Effect.gen(function*() {
      yield* empty
      const population = yield* Population.Service
      const one = "capture_facts.pcap.gz"
      yield* population.add([{ capture: one, sourcePath: "/p/one", ingested: true, extractorHash: "e1", ingestFailed: "overlap" }])
      const first = (yield* population.list)[0]!
      expect(first).toMatchObject({ extractorHash: "e1", ingestFailed: "overlap" })
      // A re-add that states nothing keeps both; one found current restates them without a new instant.
      yield* population.add([{ capture: one, sourcePath: "/p/one" }])
      expect((yield* population.list)[0]).toMatchObject({ extractorHash: "e1", ingestFailed: "overlap", ingestedAt: first.ingestedAt })
      yield* population.add([{ capture: one, sourcePath: "/p/one", extractorHash: "e2", ingestFailed: null }])
      expect((yield* population.list)[0]).toMatchObject({ extractorHash: "e2", ingestFailed: null, ingestedAt: first.ingestedAt })
      // A restated record instant never moves an instant the row holds.
      yield* population.add([{ capture: one, sourcePath: "/p/one", ingestedAt: "2020-01-01T00:00:00.000Z", extractorHash: "e2" }])
      expect((yield* population.list)[0]).toMatchObject({ ingestedAt: first.ingestedAt })
    })))

  it("fills a row's missing ingest instant from the record a current capture keeps, once", () =>
    withStore(Effect.gen(function*() {
      yield* empty
      const population = yield* Population.Service
      const kept = "capture_kept.pcap.gz"
      const at = "2026-09-19T12:00:00.000Z"
      // A store rebuilt over kept artifacts: the row is new, the record is not.
      yield* population.add([{ capture: kept, sourcePath: "/p/kept", ingestedAt: at, extractorHash: "e1", ingestFailed: null }])
      expect((yield* population.list)[0]).toMatchObject({ ingestedAt: at, extractorHash: "e1" })
      // A seeded row with no instant takes the record's on its first restatement.
      yield* population.add([{ capture: "capture_seeded.pcap.gz", sourcePath: null }])
      yield* population.add([{ capture: "capture_seeded.pcap.gz", sourcePath: "/p/seeded", ingestedAt: at, extractorHash: "e1" }])
      expect((yield* population.list)[1]).toMatchObject({ capture: "capture_seeded.pcap.gz", ingestedAt: at })
    })))

  it("seeds an empty table once and never a filled one", () =>
    withStore(Effect.gen(function*() {
      yield* empty
      const population = yield* Population.Service
      expect(yield* population.seed(["capture_a.pcap.gz", "capture_b.pcap.gz"])).toBe(2)
      expect(yield* population.seed(["capture_c.pcap.gz"])).toBe(0)
      expect(yield* population.captures).toEqual(["capture_a.pcap.gz", "capture_b.pcap.gz"])
      expect(yield* population.size).toBe(2)
    })))

  it("answers what each capture stands cut as, with the code that cut it", () =>
    withStore(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_fingerprint.pcap.gz"

      const first = yield* cuts.open(undefined, "fingerprint-one")
      yield* cuts.record({ prepId: first, capture, flows: null, cutFingerprint: "fingerprint-one", cases: [], refusals: [] })
      expect((yield* cuts.latest).get(capture)?.cutFingerprint).toBe("fingerprint-one")

      const second = yield* cuts.open(undefined, "fingerprint-two")
      yield* cuts.record({ prepId: second, capture, flows: null, cutFingerprint: "fingerprint-two", cases: [], refusals: [] })
      const standing = (yield* cuts.latest).get(capture)!
      expect(standing.cutFingerprint).toBe("fingerprint-two")
      expect(standing.prepId).toBe(second)
    })))
})
