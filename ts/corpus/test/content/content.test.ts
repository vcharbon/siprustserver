/**
 * The materialiser, against a real postgres: a case hash to a case directory.
 *
 * What is asserted is the shape a driver loads and the two properties the cache
 * rests on — a directory that exists is COMPLETE, and two materialisations of
 * one hash cannot see each other's half-written work, with no lock anywhere.
 * The suite builds and drops its OWN database, so the box's own store is
 * never touched (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { Client, Cuts, Model } from "@sip/store"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { ConnectionError, SqlError } from "effect/unstable/sql/SqlError"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest"
import * as Content from "../../src/content/content.js"
import { CaseLayout } from "../../src/deployment/index.js"

const SCRATCH = await scratch("corpus_content_test")

const withCuts = <A, E>(
  effect: Effect.Effect<A, E, Cuts.Service | SqlClient | CaseLayout.Service | FileSystem.FileSystem>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Cuts.layer),
      Effect.provide(Layer.mergeAll(Client.layerFromUrl(urlFor(SCRATCH)), CaseLayout.layer, NodeServices.layer))
    )
  )

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

const caseOf = (id: string, number: string): Model.CutCaseRequest => ({
  caseId: id,
  callKey: `${id}@key`,
  document: { case: { id }, actors: [{ kind: "uas", endpoint: "ep2" }] },
  assets: [
    { path: "resources/uac1_0_0.sdp", bytes: bytes(`v=0\r\no=${id}\r\n`) },
    { path: "resources/deep/uas1_2_0.sdp", bytes: bytes(`v=0\r\no=${id}-deep\r\n`) }
  ],
  ledger: [{ number, document: { number, dialled: "route-a" } }],
  egressEndpoint: "ep2"
})

/** The two cases the suite materialises, by content hash. */
let hashes: Record<string, string> = {}
let sessions: string

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
  hashes = await withCuts(
    Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const prep = yield* cuts.open()
      const recorded = yield* cuts.record({
        prepId: prep,
        capture: "capture_content.pcap.gz",
        flows: Model.wholeFlows(bytes(`{"capture":"capture_content.pcap.gz"}`)),
        cases: [caseOf("case-one", "1000"), caseOf("case-two", "2000")],
        refusals: []
      })
      return Object.fromEntries(recorded.cases.map((entry) => [entry.caseId, entry.caseHash]))
    })
  )
}, 120_000)

afterAll(async () => {
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

beforeEach(() => {
  sessions = fs.mkdtempSync(path.join(os.tmpdir(), "content-test-"))
})
afterEach(() => {
  fs.rmSync(sessions, { recursive: true, force: true })
})

/** Half-written work in flight, which a reader must never be handed. */
/** Half-written flows documents: the materialiser's own scratch, under `_flows`. */
const inFlightFlows = (): ReadonlyArray<string> => {
  const root = path.join(sessions, Content.CONTENT, Content.FLOWS)
  if (!fs.existsSync(root)) return []
  return fs.readdirSync(root).filter((entry) => entry.startsWith(".writing-"))
}

const inFlight = (): ReadonlyArray<string> => {
  const root = path.join(sessions, Content.CONTENT)
  if (!fs.existsSync(root)) return []
  return fs.readdirSync(root).filter((entry) => entry.startsWith(".writing-"))
}

describe.skipIf(!served)("the materialiser", () => {
  it("lays out the case directory a driver loads", async () => {
    const dir = await withCuts(Content.materialise(sessions, hashes["case-one"]!))
    // The hash names the CONTENT; the directory a cell cites is named by the
    // case's own id, because a cell id is the basename of the path it cites.
    expect(dir).toEqual(path.join(Content.caseDir(sessions, hashes["case-one"]!), "case-one"))

    expect(JSON.parse(fs.readFileSync(path.join(dir, "case-one.v3.json"), "utf8"))).toEqual({
      case: { id: "case-one" },
      actors: [{ kind: "uas", endpoint: "ep2" }]
    })
    // `scenario.json` is what `Case::load` opens, and it is the document itself.
    expect(fs.readlinkSync(path.join(dir, "scenario.json"))).toEqual("case-one.v3.json")
    expect(JSON.parse(fs.readFileSync(path.join(dir, "scenario.json"), "utf8"))).toHaveProperty("case")
    expect(fs.readFileSync(path.join(dir, "resources/uac1_0_0.sdp"), "utf8")).toContain("o=case-one")
    expect(fs.readFileSync(path.join(dir, "resources/deep/uas1_2_0.sdp"), "utf8")).toContain("deep")
    expect(JSON.parse(fs.readFileSync(path.join(dir, "rows/1000.json"), "utf8"))).toEqual({
      number: "1000",
      dialled: "route-a"
    })
    expect(inFlight()).toEqual([])
  })

  it("is idempotent: a hash already materialised is answered as it stands", async () => {
    const hash = hashes["case-one"]!
    const first = await withCuts(Content.materialise(sessions, hash))
    const stamped = fs.statSync(first).ino
    // A directory that exists is complete by construction, so it is never rebuilt.
    fs.writeFileSync(path.join(first, "proof-it-was-not-rebuilt"), "kept")

    const second = await withCuts(Content.materialise(sessions, hash))
    expect(second).toEqual(first)
    expect(fs.statSync(second).ino).toEqual(stamped)
    expect(fs.existsSync(path.join(second, "proof-it-was-not-rebuilt"))).toBe(true)
    expect(inFlight()).toEqual([])
  })

  it("never exposes a partial directory to a materialisation racing it", async () => {
    const hash = hashes["case-two"]!
    // Both fibers pass the existence check before either has read a row, so one
    // of them builds a directory the other has already published.
    const [a, b] = await withCuts(
      Effect.all(
        [Content.materialise(sessions, hash), Content.materialise(sessions, hash)],
        { concurrency: "unbounded" }
      )
    )
    expect(a).toEqual(b)
    expect(path.basename(a)).toEqual("case-two")
    expect(fs.readdirSync(a).sort()).toEqual([
      "case-two.v3.json",
      "resources",
      "rows",
      "scenario.json"
    ])
    // The loser threw its own work away rather than leaving it lying about.
    expect(inFlight()).toEqual([])
    expect(fs.readdirSync(path.join(sessions, Content.CONTENT))).toEqual([hash])
  })

  it("refuses a hash the store holds no document for", async () => {
    const outcome = await withCuts(
      Content.materialise(sessions, "0".repeat(64)).pipe(Effect.result)
    )
    expect(outcome._tag).toEqual("Failure")
    expect(inFlight()).toEqual([])
  })

  it("lays the flows document down byte for byte", async () => {
    const hash = await withCuts(
      Effect.gen(function*() {
        const cuts = yield* Cuts.Service
        return (yield* cuts.standing("capture_content.pcap.gz"))!.flowsHash!
      })
    )
    const file = await withCuts(Content.materialiseFlows(sessions, hash))
    expect(file).toEqual(Content.flowsFile(sessions, hash))
    expect(fs.readFileSync(file, "utf8")).toEqual(`{"capture":"capture_content.pcap.gz"}`)
    // Idempotent, and nothing is left in flight behind it.
    expect(await withCuts(Content.materialiseFlows(sessions, hash))).toEqual(file)
    expect(fs.readdirSync(path.join(sessions, Content.CONTENT, Content.FLOWS))).toEqual([
      `${hash}.json`
    ])
  })

  it("leaves nothing in flight when a leg it was writing cannot be read", async () => {
    // The document is written a page at a time, so a query that fails does so
    // with a file open and a scratch directory half filled.
    const hash = "1".repeat(64)
    const refuse = Effect.fail(
      new SqlError({
        reason: new ConnectionError({
          cause: new Error("the store went away"),
          message: "no connection"
        })
      })
    )
    const halfRead = Layer.succeed(
      Cuts.Service,
      Cuts.Service.of({
        open: () => refuse,
        exists: () => refuse,
        finish: () => refuse,
        record: () => refuse,
        standing: () => refuse,
        resolve: () => refuse,
        cases: () => refuse,
        standingPrep: refuse,
        latest: refuse,
        captures: () => refuse,
        caseSpans: () => refuse,
        contentFrom: () => refuse,
        content: () => refuse,
        flowsEnvelope: () =>
          Effect.succeed({ head: bytes(`{"legs":[`), tail: bytes(`]}`), legs: 1 }),
        flowsLegs: () => refuse,
        ledger: () => refuse
      })
    )
    // No descriptor is left open on anything the materialiser wrote.
    const openUnder = (dir: string) =>
      fs.readdirSync("/proc/self/fd").flatMap((fd) => {
        try {
          const target = fs.readlinkSync(`/proc/self/fd/${fd}`)
          return target.startsWith(dir) ? [target] : []
        } catch {
          return []
        }
      })
    const outcome = await Effect.runPromise(
      Content.materialiseFlows(sessions, hash).pipe(
        Effect.result,
        Effect.provide(Layer.mergeAll(halfRead, CaseLayout.layer, NodeServices.layer))
      )
    )
    expect(outcome._tag).toEqual("Failure")
    expect(openUnder(sessions)).toEqual([])
    expect(inFlightFlows()).toEqual([])
    expect(fs.existsSync(Content.flowsFile(sessions, hash))).toBe(false)
  })

  it("serves the row the store now holds when a re-cut moved only the ledger", async () => {
    // The row is a reading of the CAPTURE — a rules change re-cuts one document
    // into a different routing answer — so the cache must not go on serving the
    // row an earlier tree cut.
    const capture = "capture_reledgered.pcap.gz"
    const number = "3000"
    const cut = (dialled: string) =>
      withCuts(Effect.gen(function*() {
        const cuts = yield* Cuts.Service
        const prep = yield* cuts.open()
        const recorded = yield* cuts.record({
          prepId: prep,
          capture,
          flows: Model.wholeFlows(bytes(`{"capture":"${capture}"}`)),
          cases: [{
            ...caseOf("case-reledgered", number),
            ledger: [{ number, document: { number, dialled } }]
          }],
          refusals: []
        })
        return recorded.cases[0]!.caseHash
      }))

    const before = await cut("route-a")
    const wasDir = await withCuts(Content.materialise(sessions, before))
    const wasLedger = await withCuts(Content.materialiseLedger(sessions, [before]))

    const after = await cut("route-b")
    expect(after).not.toEqual(before)

    const isDir = await withCuts(Content.materialise(sessions, after))
    expect(isDir).not.toEqual(wasDir)
    expect(JSON.parse(fs.readFileSync(path.join(isDir, "rows", `${number}.json`), "utf8")))
      .toEqual({ number, dialled: "route-b" })
    // The merged directory a batch hands its interpreter moves with it.
    const isLedger = await withCuts(Content.materialiseLedger(sessions, [after]))
    expect(isLedger).not.toEqual(wasLedger)
    expect(JSON.parse(fs.readFileSync(path.join(isLedger, `${number}.json`), "utf8")))
      .toEqual({ number, dialled: "route-b" })

    // The old name still serves what the run driven by it actually ran.
    expect(JSON.parse(fs.readFileSync(path.join(wasDir, "rows", `${number}.json`), "utf8")))
      .toEqual({ number, dialled: "route-a" })
  })

  it("merges several cases' rows into ONE ledger directory", async () => {
    const set = [hashes["case-one"]!, hashes["case-two"]!]
    const dir = await withCuts(Content.materialiseLedger(sessions, set))
    expect(fs.readdirSync(dir).sort()).toEqual([
      "1000.json",
      "2000.json"
    ])
    // Named by the SET, so the order it is asked in does not make a second one.
    expect(await withCuts(Content.materialiseLedger(sessions, [...set].reverse()))).toEqual(dir)
    expect(JSON.parse(fs.readFileSync(path.join(dir, "2000.json"), "utf8"))).toEqual({
      number: "2000",
      dialled: "route-a"
    })
  })
})
