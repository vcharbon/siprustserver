/**
 * The cut record, against a real postgres.
 *
 * Every claim here is a claim about what the DATABASE does — a second identical
 * insert storing nothing, a prep ordering, a null distinguished from an empty
 * list — and none of them survives being asserted against a fake. The suite
 * builds and drops its OWN database, so the server's other databases are never
 * touched, and skips where no server answers.
 */
import * as Effect from "effect/Effect"
import { Buffer } from "node:buffer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import * as Client from "../src/client.js"
import * as Server from "./server.js"
import * as CaseDocuments from "../src/case-documents.js"
import * as Cuts from "../src/cuts.js"
import type * as Model from "../src/model.js"
import { type CaptureCutRequest, type CutCaseRequest, wholeFlows } from "../src/model.js"
import * as Runs from "../src/runs.js"
import { CURRENT } from "./identity.js"
import * as Sessions from "../src/sessions.js"

const SCRATCH = await Server.scratch("store_cut_test")

const urlFor = Server.urlFor

/** Runs against `database` on a pool of its own; the pool dies with the effect. */
const on = <A, E>(database: string, effect: Effect.Effect<A, E, SqlClient>): Promise<A> =>
  Effect.runPromise(effect.pipe(Effect.provide(Client.layerFromUrl(urlFor(database)))))

const withCuts = <A, E>(
  effect: Effect.Effect<A, E, Cuts.Service | Runs.Service | SqlClient>
): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Cuts.layer),
      Effect.provide(Runs.layer),
      Effect.provide(Sessions.layer("/w")),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    )
  )

const served = Server.served

const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)

/** One leg's own JSON text, and the separator carrying on to the next. */
const legOf = (index: number, last: boolean): string =>
  `{\n      "call_id": "call-${index}",\n      "msgs": []\n    }${last ? "" : ",\n    "}`

const HEAD = `{\n  "schema": 5,\n  "legs": [`
const TAIL = `],\n  "groups": []\n}\n`

/** A flows document of `legs` legs, laid out as `sipflow --enrich` lays one out. */
const document = (legs: number): string =>
  legs === 0
    ? HEAD + TAIL
    : `${HEAD}\n    ${
      Array.from({ length: legs }, (_, index) => legOf(index, index === legs - 1)).join("")
    }${TAIL}`

/** That document as the store takes it: the envelope's halves, each leg between. */
const segmented = (legs: number): Model.FlowsSource => () => {
  const parts: Array<Model.FlowsPart> = [
    { kind: "head", bytes: bytes(legs === 0 ? HEAD : `${HEAD}\n    `) }
  ]
  for (let index = 0; index < legs; index++) {
    parts.push({ kind: "leg", bytes: bytes(legOf(index, index === legs - 1)) })
  }
  parts.push({ kind: "tail", bytes: bytes(TAIL) })
  return parts
}

/** How many legs one read of `flowsLegs` asks for here; the store's own page is 64. */
const PAGE = 7

/** The document as a reader puts it back together: head, the legs in order, tail. */
const assembled = (cuts: Cuts.Interface, hash: string) =>
  Effect.gen(function*() {
    const envelope = (yield* cuts.flowsEnvelope(hash))!
    const pieces: Array<Uint8Array> = [envelope.head]
    for (let from = 0; from < envelope.legs; from += PAGE) {
      pieces.push(...(yield* cuts.flowsLegs(hash, from, PAGE)))
    }
    pieces.push(envelope.tail)
    return Buffer.concat(pieces.map((piece) => Buffer.from(piece))).toString("utf8")
  })

const caseOf = (id: string, callKey = `${id}@key`): CutCaseRequest => ({
  caseId: id,
  callKey,
  document: { id, actors: [{ kind: "uas", endpoint: "ep2" }], steps: [] },
  assets: [{ path: "resources/uac1_0_0.sdp", bytes: bytes(`v=0\r\no=${id}\r\n`) }],
  ledger: [{ number: "4242000000000001", document: { case: id, route: "site" } }],
  egressEndpoint: "ep2"
})

const cutOf = (prepId: number, capture: string, cases: ReadonlyArray<CutCaseRequest>): CaptureCutRequest => ({
  prepId,
  capture,
  flows: wholeFlows(bytes(`{"capture":"${capture}"}`)),
  cases,
  refusals: []
})

/** The content stores, which a re-cut of unchanged content must not grow. */
const stored = Effect.gen(function*() {
  const sql = yield* SqlClient
  const rows = yield* sql<
    { cases: number; documents: number; assets: number; ledger: number; flows: number }
  >`
    SELECT (SELECT count(*)::int FROM cases)           AS cases,
           (SELECT count(*)::int FROM case_documents)  AS documents,
           (SELECT count(*)::int FROM case_assets)     AS assets,
           (SELECT count(*)::int FROM ledger_rows)     AS ledger,
           (SELECT count(*)::int FROM flows_documents) AS flows
  `
  return rows[0]!
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

describe.skipIf(!served)("Cuts", () => {
  it("stores nothing new when a re-cut changed nothing", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_recut.pcap.gz"
      const cases = [caseOf(`${capture}-case1`), caseOf(`${capture}-case2`)]

      const first = yield* cuts.open()
      yield* cuts.record(cutOf(first, capture, cases))
      const after = yield* stored

      const second = yield* cuts.open()
      yield* cuts.record(cutOf(second, capture, cases))
      expect(yield* stored).toEqual(after)

      // The READING is still the new prep's own: dedupe is about content only.
      expect(yield* cuts.captures(second)).toEqual([capture])
      expect((yield* cuts.standing(capture))?.prepId).toEqual(second)
    })))

  it("stores a case again when a re-cut moved only its ledger row", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_reledgered.pcap.gz"
      const id = `${capture}-case1`
      // The row is a reading of the CAPTURE, not of the document: a rules change
      // re-cuts one document into a different routing answer.
      const before = caseOf(id)
      const after: CutCaseRequest = {
        ...before,
        ledger: [{ number: "4242000000000001", document: { case: id, route: "mrf" } }]
      }

      const first = yield* cuts.open()
      const one = yield* cuts.record(cutOf(first, capture, [before]))
      const second = yield* cuts.open()
      const two = yield* cuts.record(cutOf(second, capture, [after]))

      // A different case, stored beside the old one — never conflicted away.
      expect(two.cases[0]!.caseHash).not.toEqual(one.cases[0]!.caseHash)
      expect(two.cases[0]!.fresh).toBe(true)
      expect((yield* cuts.content(two.cases[0]!.caseHash))?.ledger).toEqual([
        { number: "4242000000000001", document: { case: id, route: "mrf" } }
      ])
      // The old name still answers what the run driven by it actually ran.
      expect((yield* cuts.content(one.cases[0]!.caseHash))?.ledger).toEqual([
        { number: "4242000000000001", document: { case: id, route: "site" } }
      ])
      // And the standing reading cites the new one, so a replay materialises it.
      expect((yield* cuts.standing(capture))?.cases[0]?.caseHash).toEqual(two.cases[0]!.caseHash)

      // One document, two cases: the re-hash costs a `cases` row, not a document.
      const sql = yield* SqlClient
      const documents = yield* sql<{ count: number }>`
        SELECT count(DISTINCT document_hash)::int AS count
        FROM cases WHERE hash IN (${one.cases[0]!.caseHash}, ${two.cases[0]!.caseHash})
      `
      expect(documents[0]!.count).toEqual(1)
    })))

  it("stores a case again when a re-cut moved only an asset", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_reasseted.pcap.gz"
      const id = `${capture}-case1`
      const before = caseOf(id)
      const after: CutCaseRequest = {
        ...before,
        assets: [{ path: "resources/uac1_0_0.sdp", bytes: bytes(`v=0\r\no=${id}-moved\r\n`) }]
      }

      const first = yield* cuts.open()
      const one = yield* cuts.record(cutOf(first, capture, [before]))
      const second = yield* cuts.open()
      const two = yield* cuts.record(cutOf(second, capture, [after]))

      expect(two.cases[0]!.caseHash).not.toEqual(one.cases[0]!.caseHash)
      expect(new TextDecoder().decode((yield* cuts.content(two.cases[0]!.caseHash))?.assets[0]?.bytes))
        .toContain("-moved")
      expect(new TextDecoder().decode((yield* cuts.content(one.cases[0]!.caseHash))?.assets[0]?.bytes))
        .not.toContain("-moved")
    })))

  it("answers the highest prep that cut a capture, or the prep it is asked for", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_standing.pcap.gz"

      const older = yield* cuts.open()
      yield* cuts.record(cutOf(older, capture, [caseOf(`${capture}-case1`)]))
      const newer = yield* cuts.open()
      yield* cuts.record(
        cutOf(newer, capture, [caseOf(`${capture}-case1`), caseOf(`${capture}-case2`)])
      )

      const standing = yield* cuts.standing(capture)
      expect(standing?.prepId).toEqual(newer)
      expect(standing?.cases.map((entry) => entry.caseId)).toEqual([
        `${capture}-case1`,
        `${capture}-case2`
      ])
      expect(standing?.cases[0]?.egressEndpoint).toEqual("ep2")
      expect(standing?.cases[0]?.callKey).toEqual(`${capture}-case1@key`)
      expect(standing?.flowsHash).not.toBeNull()

      const named = yield* cuts.standing(capture, older)
      expect(named?.prepId).toEqual(older)
      expect(named?.cases.map((entry) => entry.caseId)).toEqual([`${capture}-case1`])

      expect(yield* cuts.standingPrep).toEqual(newer)
      expect(yield* cuts.standing("capture_never_cut.pcap.gz")).toBeNull()
    })))

  it("drops a case a re-cut of the same prep no longer makes", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_dropped.pcap.gz"
      const prep = yield* cuts.open()
      yield* cuts.record(cutOf(prep, capture, [caseOf(`${capture}-case1`), caseOf(`${capture}-case2`)]))
      yield* cuts.record(cutOf(prep, capture, [caseOf(`${capture}-case1`)]))
      expect((yield* cuts.standing(capture))?.cases.map((entry) => entry.caseId)).toEqual([
        `${capture}-case1`
      ])
    })))

  it("keeps a broken cut apart from one that refused every case", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const prep = yield* cuts.open()
      yield* cuts.record({
        prepId: prep,
        capture: "capture_broke.pcap.gz",
        flows: null,
        broke: "[prep] enrich failed",
        cases: [],
        refusals: []
      })
      yield* cuts.record(cutOf(prep, "capture_refused.pcap.gz", []))

      const broke = yield* cuts.standing("capture_broke.pcap.gz")
      expect(broke?.broke).toEqual("[prep] enrich failed")
      expect(broke?.cases).toEqual([])
      expect(broke?.flowsHash).toBeNull()

      const refused = yield* cuts.standing("capture_refused.pcap.gz")
      expect(refused?.broke).toBeNull()
      expect(refused?.cases).toEqual([])

      expect(yield* cuts.captures(prep)).toEqual([
        "capture_broke.pcap.gz",
        "capture_refused.pcap.gz"
      ])
    })))

  it("spans each standing case of a capture, null where the case states none", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const spanned = (id: string, ms: number | null): CutCaseRequest => ({
        ...caseOf(id),
        document: { id, actors: [], steps: [], timing: ms === null ? {} : { capture_span_ms: ms } }
      })

      const older = yield* cuts.open()
      yield* cuts.record(cutOf(older, "capture_span.pcap.gz", [spanned("span-long", 400_000)]))
      const newer = yield* cuts.open()
      yield* cuts.record(
        cutOf(newer, "capture_span.pcap.gz", [
          spanned("span-short", 5_000),
          spanned("span-wide", 42_000),
          spanned("span-none", null)
        ])
      )
      yield* cuts.record(cutOf(newer, "capture_untimed.pcap.gz", [spanned("untimed-only", null)]))

      // Every case of the STANDING cut, each with its own span; the older cut's case is gone.
      const standing = yield* cuts.caseSpans()
      expect([...standing.get("capture_span.pcap.gz")!]).toEqual([
        ["span-none", null],
        ["span-short", 5_000],
        ["span-wide", 42_000]
      ])
      // Unknown is not short: a case that states no span is null, never 0.
      expect([...standing.get("capture_untimed.pcap.gz")!]).toEqual([["untimed-only", null]])

      // The prep is a CEILING, so an older reading answers with what it cut.
      expect([...(yield* cuts.caseSpans(older)).get("capture_span.pcap.gz")!]).toEqual([["span-long", 400_000]])
    })))

  it("reads one field of each case's document by case hash, a case lacking it absent", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const documents = yield* CaseDocuments.Service
      const grouped = (id: string, groups: ReadonlyArray<number> | null): CutCaseRequest => ({
        ...caseOf(id),
        document: { id, case: groups === null ? {} : { source: { call_groups: groups } } }
      })
      const prep = yield* cuts.open()
      yield* cuts.record(cutOf(prep, "capture_groups.pcap.gz", [grouped("g-one", [3]), grouped("g-none", null)]))
      const cases = (yield* cuts.standing("capture_groups.pcap.gz"))!.cases
      const hashOf = (id: string) => cases.find((entry) => entry.caseId === id)!.caseHash
      const read = yield* documents.fieldOfCases(cases.map((entry) => entry.caseHash), ["case", "source", "call_groups"])
      expect([...read]).toEqual([[hashOf("g-one"), [3]]])
    }).pipe(Effect.provide(CaseDocuments.layer))))

  it("keeps every content hash a reading from the floor prep up can name", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_live.pcap.gz"

      const older = yield* cuts.open()
      const first = yield* cuts.record(cutOf(older, capture, [caseOf("live-dropped")]))
      const newer = yield* cuts.open()
      const second = yield* cuts.record(cutOf(newer, capture, [caseOf("live-kept")]))
      const dropped = first.cases[0]!.caseHash
      const kept = second.cases[0]!.caseHash

      // Read as of the newest cut: the case the re-cut dropped is nobody's,
      // and neither is the cut that made it — its log has nothing left to
      // explain.
      const now = yield* cuts.contentFrom(newer)
      expect(now.cases.has(kept)).toBe(true)
      expect(now.cases.has(dropped)).toBe(false)
      expect(now.flows.size).toBeGreaterThan(0)
      expect(now.preps.has(newer)).toBe(true)
      expect(now.preps.has(older)).toBe(false)

      // A command still pinned to the older cut floors the sweep there, and
      // everything from that cut UP survives — both readings, not just its own.
      const floored = yield* cuts.contentFrom(older)
      expect(floored.cases.has(kept)).toBe(true)
      expect(floored.cases.has(dropped)).toBe(true)
      expect(floored.preps.has(older)).toBe(true)
      expect(floored.preps.has(newer)).toBe(true)
    })))

  it("keeps a capture whose only cut is older than the floor", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const only = yield* cuts.open()
      const recorded = yield* cuts.record(
        cutOf(only, "capture_cut_once.pcap.gz", [caseOf("cut-once")])
      )
      const later = yield* cuts.open()
      yield* cuts.record(cutOf(later, "capture_elsewhere.pcap.gz", [caseOf("elsewhere")]))

      // The floor is applied per capture: this one still STANDS at its old cut.
      const live = yield* cuts.contentFrom(later)
      expect(live.cases.has(recorded.cases[0]!.caseHash)).toBe(true)
    })))

  it("names a case document exactly as a run does", () =>
    withCuts(Effect.gen(function*() {
      const sql = yield* SqlClient
      const cuts = yield* Cuts.Service
      const runs = yield* Runs.Service
      const capture = "capture_shared.pcap.gz"
      const caseId = `${capture}-case1`

      const run = yield* runs.open({ capture, lane: "fake", mode: "record", sessionDir: "/w/s", identity: CURRENT })
      yield* runs.finish({
        id: run.id,
        status: "done",
        cases: [{
          caseId,
          passed: true,
          verdict: "ok",
          skipped: null,
          failedStep: null,
          rfcGating: null,
          mainError: null,
          classification: null,
          // Same document, keys the other way round: the cut and the replay
          // must land on one row, which is what stage 2's free check reads.
          document: { steps: [], actors: [{ endpoint: "ep2", kind: "uas" }], id: caseId }
        }]
      })

      const prep = yield* cuts.open()
      yield* cuts.record(cutOf(prep, capture, [caseOf(caseId)]))

      const rows = yield* sql<{ document_hash: string; cut_document_hash: string; documents: number }>`
        SELECT cr.document_hash, c.document_hash AS cut_document_hash,
               (SELECT count(*)::int FROM case_documents d WHERE d.hash = c.document_hash) AS documents
        FROM case_results cr
        JOIN cut_cases cc ON cc.case_id = cr.case_id
        JOIN cases c ON c.hash = cc.case_hash
        WHERE cr.case_id = ${caseId}
      `
      expect(rows).toHaveLength(1)
      expect(rows[0]!.cut_document_hash).toEqual(rows[0]!.document_hash)
      expect(rows[0]!.documents).toEqual(1)
    })))

  it("records what the cut refused beside what it made", () =>
    withCuts(Effect.gen(function*() {
      const sql = yield* SqlClient
      const cuts = yield* Cuts.Service
      const capture = "capture_refusals.pcap.gz"
      const prep = yield* cuts.open()
      yield* cuts.record({
        ...cutOf(prep, capture, [caseOf(`${capture}-case1`)]),
        refusals: [
          { caseId: `${capture}-cut2`, reasons: ["egress-answer-not-captured"], callIds: ["a@x", "b@x"], detail: "leg 2 hop 1" },
          { caseId: `${capture}-cut3`, reasons: ["source-call-incomplete"], callIds: [], detail: "no final" }
        ]
      })
      const rows = yield* sql<{ case_id: string; reasons: ReadonlyArray<string>; call_ids: ReadonlyArray<string>; detail: string }>`
        SELECT case_id, reasons, call_ids, detail FROM cut_refusals
        WHERE prep_id = ${prep} AND capture = ${capture} ORDER BY case_id
      `
      expect(rows).toEqual([
        { case_id: `${capture}-cut2`, reasons: ["egress-answer-not-captured"], call_ids: ["a@x", "b@x"], detail: "leg 2 hop 1" },
        { case_id: `${capture}-cut3`, reasons: ["source-call-incomplete"], call_ids: [], detail: "no final" }
      ])
    })))

  it("keeps every reason a call was refused under, as one row of that call", () =>
    withCuts(Effect.gen(function*() {
      const sql = yield* SqlClient
      const cuts = yield* Cuts.Service
      const capture = "capture_twice_refused.pcap.gz"
      const prep = yield* cuts.open()
      // One record per refusing verdict, as `excluded.json` holds them.
      yield* cuts.record({
        ...cutOf(prep, capture, []),
        refusals: [
          { caseId: `${capture}-auto`, reasons: ["egress-answer-not-captured"], callIds: ["a@x"], detail: "first" },
          { caseId: `${capture}-auto`, reasons: ["source-call-incomplete"], callIds: ["a@x", "b@x"], detail: "second" }
        ]
      })
      const rows = yield* sql<{ case_id: string; reasons: ReadonlyArray<string>; call_ids: ReadonlyArray<string>; detail: string }>`
        SELECT case_id, reasons, call_ids, detail FROM cut_refusals
        WHERE prep_id = ${prep} AND capture = ${capture}
      `
      expect(rows).toEqual([{
        case_id: `${capture}-auto`,
        reasons: ["egress-answer-not-captured", "source-call-incomplete"],
        call_ids: ["a@x", "b@x"],
        detail: "first; second"
      }])
    })))

  it("replaces the refusals on a re-record, so a refusal the re-cut lifted does not stand", () =>
    withCuts(Effect.gen(function*() {
      const sql = yield* SqlClient
      const cuts = yield* Cuts.Service
      const capture = "capture_rerefused.pcap.gz"
      const prep = yield* cuts.open()
      const refusing = (reason: string) => ({
        ...cutOf(prep, capture, []),
        refusals: [{ caseId: `${capture}-cut1`, reasons: [reason], callIds: [], detail: reason }]
      })
      yield* cuts.record(refusing("source-truncated-at-edge"))
      yield* cuts.record(refusing("egress-answer-not-captured"))
      const rows = yield* sql<{ reasons: ReadonlyArray<string> }>`
        SELECT reasons FROM cut_refusals WHERE prep_id = ${prep} AND capture = ${capture}
      `
      expect(rows).toEqual([{ reasons: ["egress-answer-not-captured"] }])

      yield* cuts.record(cutOf(prep, capture, [caseOf(`${capture}-case1`)]))
      const lifted = yield* sql<{ reasons: ReadonlyArray<string> }>`
        SELECT reasons FROM cut_refusals WHERE prep_id = ${prep} AND capture = ${capture}
      `
      expect(lifted).toEqual([])
    })))

  it("drops the refusals with the cut they belong to", () =>
    withCuts(Effect.gen(function*() {
      const sql = yield* SqlClient
      const cuts = yield* Cuts.Service
      const capture = "capture_cascaded.pcap.gz"
      const prep = yield* cuts.open()
      yield* cuts.record({
        ...cutOf(prep, capture, []),
        refusals: [{ caseId: `${capture}-cut1`, reasons: ["source-call-incomplete"], callIds: [], detail: "x" }]
      })
      yield* sql`DELETE FROM capture_cuts WHERE prep_id = ${prep} AND capture = ${capture}`
      const rows = yield* sql<{ reasons: ReadonlyArray<string> }>`
        SELECT reasons FROM cut_refusals WHERE prep_id = ${prep} AND capture = ${capture}
      `
      expect(rows).toEqual([])
    })))

  it("keeps the flows document per leg and gives it back byte for byte", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const sql = yield* SqlClient
      const capture = "capture_perleg.pcap.gz"
      const prep = yield* cuts.open()
      yield* cuts.record({
        prepId: prep,
        capture,
        flows: segmented(3),
        cases: [],
        refusals: []
      })

      const hash = (yield* cuts.standing(capture))!.flowsHash!
      const rows = yield* sql<{ legs: number }>`
        SELECT count(*)::int AS legs FROM flows_legs WHERE flows_hash = ${hash}
      `
      expect(rows[0]!.legs).toEqual(3)

      const envelope = yield* cuts.flowsEnvelope(hash)
      expect(envelope?.legs).toEqual(3)
      expect(yield* assembled(cuts, hash)).toEqual(document(3))
    })))

  it("rebuilds every document it was given, whatever it held", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const prep = yield* cuts.open()
      // 130 is more legs than one insert carries, so the paging is under test too.
      for (const legs of [0, 1, 2, 17, 130]) {
        const capture = `capture_legs${legs}.pcap.gz`
        yield* cuts.record({ prepId: prep, capture, flows: segmented(legs), cases: [], refusals: [] })
        const hash = (yield* cuts.standing(capture))!.flowsHash!
        expect(yield* assembled(cuts, hash)).toEqual(document(legs))
      }
    })))

  it("re-stores no leg of a document the store already holds", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const sql = yield* SqlClient
      const capture = "capture_reflowed.pcap.gz"
      const flows = segmented(4)
      const first = yield* cuts.open()
      yield* cuts.record({ prepId: first, capture, flows, cases: [], refusals: [] })
      const before = yield* sql<{ legs: number }>`SELECT count(*)::int AS legs FROM flows_legs`
      const second = yield* cuts.open()
      yield* cuts.record({ prepId: second, capture, flows, cases: [], refusals: [] })
      expect(yield* sql<{ legs: number }>`SELECT count(*)::int AS legs FROM flows_legs`)
        .toEqual(before)
    })))
})

describe.skipIf(!served)("resolving call keys against a cut", () => {
  it("names the case a key stands for, a gone key by the refusal's reasons, and a key nothing names as not in this cut", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_resolve.pcap.gz"
      const prep = yield* cuts.open()
      yield* cuts.record({
        ...cutOf(prep, capture, [
          caseOf(`${capture}-cut1`, "SDroot-a@x"),
          caseOf(`${capture}-cut2`, "SDroot-b@x#ingress0"),
          caseOf(`${capture}-cut3`, "SDroot-b@x#ingress1")
        ]),
        refusals: [{
          caseId: `${capture}-cut4`,
          reasons: ["source-truncated-at-edge", "source-call-incomplete"],
          callIds: ["SDroot-c@x", "1-SDroot-c@x"],
          detail: "edge"
        }]
      })

      const resolved = yield* cuts.resolve(prep, capture, [
        "SDroot-a@x",
        "SDroot-b@x#ingress1",
        "SDroot-c@x",
        "SDroot-c@x#ingress1",
        "SDroot-never@x"
      ])
      expect(resolved.get("SDroot-a@x")).toEqual({
        caseId: `${capture}-cut1`,
        callKey: "SDroot-a@x",
        caseHash: expect.any(String),
        egressEndpoint: "ep2"
      })
      expect(resolved.get("SDroot-b@x#ingress1")).toMatchObject({ caseId: `${capture}-cut3` })
      // A qualified key of a family the re-cut refused whole folds to its Call-ID.
      const refused = { gone: "refused", reasons: ["source-truncated-at-edge", "source-call-incomplete"] }
      expect(resolved.get("SDroot-c@x")).toEqual(refused)
      expect(resolved.get("SDroot-c@x#ingress1")).toEqual(refused)
      expect(resolved.get("SDroot-never@x")).toEqual({ gone: "not-in-this-cut", reasons: [] })
      expect(resolved.size).toEqual(5)
    })))

  it("reads the cut standing at the prep ceiling, and answers every key outside a capture no prep cut", () =>
    withCuts(Effect.gen(function*() {
      const cuts = yield* Cuts.Service
      const capture = "capture_resolve_ceiling.pcap.gz"
      const older = yield* cuts.open()
      yield* cuts.record(cutOf(older, capture, [caseOf(`${capture}-cut1`, "SDold@x")]))
      const newer = yield* cuts.open()
      yield* cuts.record({
        ...cutOf(newer, capture, []),
        refusals: [{ caseId: `${capture}-cut1`, reasons: ["source-call-incomplete"], callIds: ["SDold@x"], detail: "" }]
      })

      expect((yield* cuts.resolve(older, capture, ["SDold@x"])).get("SDold@x")).toMatchObject({
        caseId: `${capture}-cut1`
      })
      expect((yield* cuts.resolve(newer, capture, ["SDold@x"])).get("SDold@x")).toEqual({
        gone: "refused",
        reasons: ["source-call-incomplete"]
      })
      expect(yield* cuts.resolve(newer, "capture_never_cut.pcap.gz", ["SDold@x"])).toEqual(
        new Map([["SDold@x", { gone: "not-in-this-cut", reasons: [] }]])
      )
      expect((yield* cuts.resolve(newer, capture, [])).size).toEqual(0)
    })))
})
