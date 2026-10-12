/**
 * The CUT record: which prep cut which capture, what it made of it, and the
 * content those cases rebuild from.
 *
 * Everything stored is content-named, so a re-cut that changed nothing writes
 * nothing: the case, document, asset and ledger inserts all conflict onto rows
 * that already say the same thing. A case is named over everything it CARRIES,
 * document and assets and ledger rows alike, so a re-cut that moved only the
 * row is a new case rather than a conflict. Only the READING is prep-scoped,
 * because a re-cut may legitimately make a different case list of the same
 * capture — and a prep is re-read whole, never merged, so a case the cut
 * dropped does not stand.
 *
 * Like the run record, this holds no judgment: it says what a cut produced,
 * never what a failure means.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import { Buffer } from "node:buffer"
import { CallKey } from "@sip/contracts"
import { hashCase, hashChunks } from "./hash.js"
import type {
  CaptureCut,
  CaptureCutRequest,
  CaptureCutState,
  CaseAsset,
  CaseContent,
  CutCase,
  CutCaseRequest,
  FlowsEnvelope,
  FlowsSource,
  LedgerRow,
  Provenance,
  Recorded,
  RecordedCase,
  Resolved
} from "./model.js"
import { UNKNOWN_PROVENANCE } from "./model.js"
import * as CaseIds from "./case-ids.js"
import * as CaseMaps from "./case-maps.js"
import * as Refusals from "./refusals.js"

export interface Interface {
  /**
   * Opens a cut invocation and answers its id; the prep number comes from the
   * sequence. The fingerprint is the cut CODE this prep runs, which every
   * reading it takes carries with it; `jobId` names the cut job opening it,
   * which is what that job's progress is counted under.
   */
  readonly open: (
    provenance?: Provenance,
    cutFingerprint?: string | null,
    jobId?: number
  ) => Effect.Effect<number, SqlError>
  readonly finish: (prepId: number) => Effect.Effect<void, SqlError>
  /** Whether a prep exists at all, which is what a resumed cut asks before re-using one. */
  readonly exists: (prepId: number) => Effect.Effect<boolean, SqlError>
  /**
   * One capture's cut, whole or not at all: no reader ever sees half a reading.
   * Answers what it STORED — every case's hash, and whether the document was
   * new to the store. The cut's refusals are written with it (`refusals.ts`),
   * and so are its case map (`case-maps.ts`) and its frozen ids
   * (`case-ids.ts`): a reading naming a frozen call or id otherwise fails
   * `CaseIds.Conflict` and writes nothing.
   */
  readonly record: (request: CaptureCutRequest) => Effect.Effect<Recorded, CaseIds.Conflict | SqlError>
  /**
   * What a capture stands cut as AS OF a prep: the highest prep no later than
   * the named one that cut it, or the highest of all where none is named;
   * `null` where no prep cut it. The named prep is a CEILING rather than an
   * exact match, because a command pins ONE prep for captures that were cut in
   * different ones — a targeted re-cut of six captures must not un-cut the
   * corpus behind it. `broke` non-null failed, `broke` null with no cases is
   * `no-cases`.
   */
  readonly standing: (capture: string, prepId?: number) => Effect.Effect<CaptureCut | null, SqlError>
  /**
   * What each call key names in a capture's cut AS OF a prep — the same
   * ceiling {@link Interface.standing} reads by — in one query: the case it
   * names, or why it names none (`refused` with the refusal's reasons where
   * the cut's refusals name the key's Call-ID, `not-in-this-cut` otherwise). A
   * key the map does not hold was not asked for. This is the lookup every
   * replay of a request makes.
   */
  readonly resolve: (
    prepId: number,
    capture: string,
    keys: ReadonlyArray<string>
  ) => Effect.Effect<ReadonlyMap<string, Resolved>, SqlError>
  /**
   * The cases of many captures' cuts AS OF a prep — the ceiling
   * {@link Interface.standing} reads by — in one query, per capture in case-id
   * order. A capture no prep at the ceiling cut is absent from the map. This
   * is what freezes a whole-capture request and what says whether a request
   * holds a capture whole, without materialising a case.
   */
  readonly cases: (
    prepId: number,
    captures: ReadonlyArray<string>
  ) => Effect.Effect<ReadonlyMap<string, ReadonlyArray<CutCase>>, SqlError>
  /** The highest prep that cut anything, which is what a command pins itself to. */
  readonly standingPrep: Effect.Effect<number | null, SqlError>
  /**
   * What each capture stands cut as, cases left out: the newest reading of it,
   * whatever prep took it. This is what a campaign asks to decide whether a cut
   * is still current, so it answers for the WHOLE store in one query rather
   * than a `standing` call per capture.
   */
  readonly latest: Effect.Effect<ReadonlyMap<string, CaptureCutState>, SqlError>
  /** Every capture a prep cut, broken ones included. */
  readonly captures: (prepId: number) => Effect.Effect<ReadonlyArray<string>, SqlError>
  /**
   * What each standing case SPANS, in milliseconds, as of a prep ceiling: per
   * capture, every case of its standing cut with its `timing.capture_span_ms`.
   * Per case, because a span bound rules per call (`Population.withinSpan`)
   * and a capture is replayed as the calls under it. A case that states no
   * span is `null`, never zero: unknown is not short.
   */
  readonly caseSpans: (
    prepId?: number
  ) => Effect.Effect<ReadonlyMap<string, ReadonlyMap<string, number | null>>, SqlError>
  /**
   * Everything a reading no lower than `floorPrep` can cite: for each capture,
   * the cut standing at that prep and every cut of it since, answered as the
   * content hashes those cuts name and the preps that made them. It is a
   * SUPERSET of what any one reading cites, because a retention pass needs the
   * cheapest safe answer rather than the exact one — naming the exact set costs
   * one standing query per live ceiling, and over-keeping a case directory costs
   * 40 KB until the next pass.
   */
  readonly contentFrom: (floorPrep: number) => Effect.Effect<Live, SqlError>
  /** Everything one case materialises from; `null` where the store holds no such document. */
  readonly content: (caseHash: string) => Effect.Effect<CaseContent | null, SqlError>
  /**
   * A stored document's envelope alone, and how many legs stand between its
   * halves: what a reader asks before assembling a document it must never hold
   * whole. `null` where the store holds no such document. With
   * {@link Interface.flowsLegs} it is the WHOLE way out of the store: the
   * document is head, the legs in order, then tail, byte for byte what the cut
   * read.
   */
  readonly flowsEnvelope: (flowsHash: string) => Effect.Effect<FlowsEnvelope | null, SqlError>
  /** One window of a stored document's legs, in document order. */
  readonly flowsLegs: (
    flowsHash: string,
    from: number,
    limit: number
  ) => Effect.Effect<ReadonlyArray<Uint8Array>, SqlError>
  /**
   * The ledger rows of several cases at once, which is what one
   * merged ledger directory holds. A number belongs to one case, so the rows
   * of a set of cases never collide.
   */
  readonly ledger: (caseHashes: ReadonlyArray<string>) => Effect.Effect<ReadonlyArray<LedgerRow>, SqlError>
}

/** What the store still stands behind: the content a case rebuilds from, and the cuts that named it. */
export interface Live {
  readonly cases: ReadonlySet<string>
  readonly flows: ReadonlySet<string>
  /** The preps those readings belong to, which is what still owns a cut log. */
  readonly preps: ReadonlySet<number>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Cuts") {}

const EMPTY = new Uint8Array(0)

/** What a document is stored under, and the envelope it is stored with: one pass. */
const named = (source: FlowsSource): {
  readonly hash: string
  readonly head: Uint8Array
  readonly tail: Uint8Array
} => {
  let head: Uint8Array = EMPTY
  let tail: Uint8Array = EMPTY
  const hash = hashChunks({
    *[Symbol.iterator]() {
      for (const part of source()) {
        if (part.kind === "head") head = part.bytes
        else if (part.kind === "tail") tail = part.bytes
        yield part.bytes
      }
    }
  })
  return { hash, head, tail }
}

/** How many legs one query carries: a leg is ~100 KB, a window is a few MB. */
const FLOWS_PAGE = 64

interface CutRow {
  readonly prep_id: string
  readonly capture: string
  readonly flows_hash: string | null
  readonly broke: string | null
  readonly cut_at: Date
  readonly cases: ReadonlyArray<CutCase>
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    /**
     * The case, named over everything it carries, answering that name. Every
     * insert conflicts onto content that is already identical, so this is a
     * no-op for a case the corpus has already seen — and a case whose assets or
     * ledger rows MOVED is a different name, stored beside the old one rather
     * than conflicted away.
     */
    const writeCase = Effect.fn("Cuts.writeCase")(function*(entry: CutCaseRequest) {
      const { documentHash, documentText, hash } = hashCase(entry)
      yield* sql`
        INSERT INTO case_documents (hash, document)
        VALUES (${documentHash}, ${documentText}::jsonb)
        ON CONFLICT (hash) DO NOTHING
      `
      // The insert's own answer to whether the store had this case: a conflicted
      // row returns nothing, which is the agreement a re-cut reports.
      const stored = yield* sql<{ hash: string }>`
        INSERT INTO cases (hash, document_hash)
        VALUES (${hash}, ${documentHash})
        ON CONFLICT (hash) DO NOTHING
        RETURNING hash
      `
      for (const asset of entry.assets) {
        yield* sql`
          INSERT INTO case_assets (case_hash, path, bytes)
          VALUES (${hash}, ${asset.path}, ${Buffer.from(asset.bytes)})
          ON CONFLICT (case_hash, path) DO NOTHING
        `
      }
      for (const row of entry.ledger) {
        yield* sql`
          INSERT INTO ledger_rows (case_hash, number, document)
          VALUES (${hash}, ${row.number}, ${JSON.stringify(row.document)}::jsonb)
          ON CONFLICT (case_hash, number) DO NOTHING
        `
      }
      return { caseId: entry.caseId, caseHash: hash, fresh: stored.length === 1 } satisfies RecordedCase
    })

    /**
     * The flows document, kept as the pieces it arrived in, answering the name
     * it is stored under. A document the store already holds is already whole,
     * so its legs are not offered twice: the second pass over a gigabyte is the
     * one this saves.
     */
    const storeFlows = Effect.fn("Cuts.storeFlows")(function*(source: FlowsSource) {
      const { hash, head, tail } = named(source)
      const stored = yield* sql<{ hash: string }>`
        INSERT INTO flows_documents (hash, head, tail)
        VALUES (${hash}, ${Buffer.from(head)}, ${Buffer.from(tail)})
        ON CONFLICT (hash) DO NOTHING
        RETURNING hash
      `
      if (stored.length === 0) return hash
      // A page at a time: 60,000 legs is 60,000 round trips one row at a time.
      let page: Array<{ flows_hash: string; idx: number; bytes: Buffer }> = []
      let index = 0
      for (const part of source()) {
        if (part.kind !== "leg") continue
        page.push({ flows_hash: hash, idx: index++, bytes: Buffer.from(part.bytes) })
        if (page.length < FLOWS_PAGE) continue
        yield* sql`INSERT INTO flows_legs ${sql.insert(page)}`
        page = []
      }
      if (page.length > 0) yield* sql`INSERT INTO flows_legs ${sql.insert(page)}`
      return hash
    })

    const open = Effect.fn("Cuts.open")(function*(
      provenance?: Provenance,
      cutFingerprint?: string | null,
      jobId?: number
    ) {
      const p = provenance ?? UNKNOWN_PROVENANCE
      const rows = yield* sql<{ id: string }>`
        INSERT INTO preps (components, cut_fingerprint, job_id)
        VALUES (${JSON.stringify(p)}::jsonb, ${cutFingerprint ?? null}, ${jobId ?? null})
        RETURNING id
      `
      return Number(rows[0]!.id)
    })

    const exists = Effect.fn("Cuts.exists")(function*(prepId: number) {
      const rows = yield* sql<{ id: string }>`SELECT id FROM preps WHERE id = ${prepId}`
      return rows.length === 1
    })

    const finish = Effect.fn("Cuts.finish")(function*(prepId: number) {
      yield* sql`UPDATE preps SET finished_at = now() WHERE id = ${prepId}`
    })

    const record = Effect.fn("Cuts.record")(function*(request: CaptureCutRequest) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        let flowsHash: string | null = null
        if (request.flows !== null && request.flows !== undefined) {
          flowsHash = yield* storeFlows(request.flows)
        }
        const cut: Array<readonly [CutCaseRequest, RecordedCase]> = []
        for (const entry of request.cases) cut.push([entry, yield* writeCase(entry)])

        yield* sql`
          INSERT INTO capture_cuts (prep_id, capture, flows_hash, broke, cut_fingerprint)
          VALUES (${request.prepId}, ${request.capture}, ${flowsHash}, ${request.broke ?? null},
                  ${request.cutFingerprint ?? null})
          ON CONFLICT (prep_id, capture) DO UPDATE SET
            flows_hash = EXCLUDED.flows_hash,
            broke = EXCLUDED.broke,
            cut_fingerprint = EXCLUDED.cut_fingerprint,
            cut_at = now()
        `
        // Replaced, not merged: a re-cut that dropped a case must not leave the
        // dropped one standing as this prep's reading of the capture.
        yield* sql`
          DELETE FROM cut_cases
          WHERE prep_id = ${request.prepId} AND capture = ${request.capture}
        `
        for (const [entry, recorded] of cut) {
          yield* sql`
            INSERT INTO cut_cases (prep_id, capture, case_id, call_key, case_hash, egress_endpoint)
            VALUES (${request.prepId}, ${request.capture}, ${entry.caseId}, ${entry.callKey},
                    ${recorded.caseHash}, ${entry.egressEndpoint ?? null})
          `
        }
        // What the cut refused stands and falls with the reading it belongs to.
        yield* Refusals.write(sql, request.prepId, request.capture, request.refusals)
        yield* CaseMaps.writeReading(sql, request.prepId, request.capture)
        // Every id the reading names freezes with it; one naming a frozen
        // call or id otherwise refuses the reading whole.
        yield* CaseIds.freezeOn(
          sql,
          request.capture,
          [
            ...request.cases.map((entry) => ({ caseId: entry.caseId, callKey: entry.callKey })),
            ...request.refusals.flatMap((refusal) =>
              refusal.callKey === undefined ? [] : [{ caseId: refusal.caseId, callKey: refusal.callKey }]
            )
          ],
          `prep${request.prepId}`
        )
        return { cases: cut.map(([, recorded]) => recorded) } satisfies Recorded
      }))
    })

    const standing = Effect.fn("Cuts.standing")(function*(capture: string, prepId?: number) {
      // The capture's cases come back on the reading itself: a caller asking
      // what to materialise never has a use for the head row alone.
      const rows = yield* sql<CutRow>`
        SELECT c.prep_id, c.capture, c.flows_hash, c.broke, c.cut_at,
               coalesce((
                 SELECT jsonb_agg(
                          jsonb_build_object(
                            'caseId', cc.case_id,
                            'callKey', cc.call_key,
                            'caseHash', cc.case_hash,
                            'egressEndpoint', cc.egress_endpoint
                          ) ORDER BY cc.case_id
                        )
                 FROM cut_cases cc
                 WHERE cc.prep_id = c.prep_id AND cc.capture = c.capture
               ), '[]'::jsonb) AS cases
        FROM capture_cuts c
        WHERE c.capture = ${capture}
          AND ${prepId === undefined ? sql`true` : sql`c.prep_id <= ${prepId}`}
        ORDER BY c.prep_id DESC
        LIMIT 1
      `
      const row = rows[0]
      if (row === undefined) return null
      return {
        prepId: Number(row.prep_id),
        capture: row.capture,
        flowsHash: row.flows_hash,
        broke: row.broke,
        cutAt: row.cut_at.toISOString(),
        cases: row.cases ?? []
      } satisfies CaptureCut
    })

    const resolve = Effect.fn("Cuts.resolve")(function*(
      prepId: number,
      capture: string,
      keys: ReadonlyArray<string>
    ) {
      const resolved = new Map<string, Resolved>()
      if (keys.length === 0) return resolved as ReadonlyMap<string, Resolved>
      const wanted = [...new Set(keys)]
      // The keys and their Call-IDs travel as one JSON list: a JS array is a
      // value list to the statement builder, not a postgres array. A refusal is
      // consulted only for a key no case holds, through the Call-IDs it names
      // (`cut_refusal_calls`), the first in case order where several do.
      const cells = JSON.stringify(wanted.map((key) => ({ key, callId: CallKey.callIdOf(key) })))
      const rows = yield* sql<{
        call_key: string
        case_id: string | null
        case_hash: string | null
        egress_endpoint: string | null
        reasons: ReadonlyArray<string> | null
      }>`
        WITH standing AS (
          SELECT prep_id FROM capture_cuts
          WHERE capture = ${capture} AND prep_id <= ${prepId}
          ORDER BY prep_id DESC
          LIMIT 1
        ),
        wanted AS (
          SELECT w.key AS call_key, w."callId" AS call_id
          FROM jsonb_to_recordset(${cells}::jsonb) AS w(key text, "callId" text)
        )
        SELECT w.call_key, c.case_id, c.case_hash, c.egress_endpoint, r.reasons
        FROM wanted w
        CROSS JOIN standing s
        LEFT JOIN cut_cases c
          ON c.prep_id = s.prep_id AND c.capture = ${capture} AND c.call_key = w.call_key
        LEFT JOIN LATERAL (
          SELECT r.reasons
          FROM cut_refusal_calls rc
          JOIN cut_refusals r
            ON r.prep_id = rc.prep_id AND r.capture = rc.capture AND r.case_id = rc.case_id
          WHERE c.case_id IS NULL
            AND rc.prep_id = s.prep_id AND rc.capture = ${capture} AND rc.call_id = w.call_id
          ORDER BY rc.case_id
          LIMIT 1
        ) r ON true
      `
      for (const row of rows) {
        resolved.set(
          row.call_key,
          row.case_id !== null && row.case_hash !== null
            ? {
              caseId: row.case_id,
              callKey: row.call_key,
              caseHash: row.case_hash,
              egressEndpoint: row.egress_endpoint
            } satisfies CutCase
            : row.reasons !== null
            ? { gone: "refused", reasons: row.reasons }
            : { gone: "not-in-this-cut", reasons: [] }
        )
      }
      // A capture no prep at the ceiling cut answers no row at all: every key is outside it.
      for (const key of wanted) {
        if (!resolved.has(key)) resolved.set(key, { gone: "not-in-this-cut", reasons: [] })
      }
      return resolved as ReadonlyMap<string, Resolved>
    })

    const cases = Effect.fn("Cuts.cases")(function*(prepId: number, captures: ReadonlyArray<string>) {
      const found = new Map<string, Array<CutCase>>()
      if (captures.length === 0) return found as ReadonlyMap<string, ReadonlyArray<CutCase>>
      const wanted = JSON.stringify([...new Set(captures)])
      const rows = yield* sql<{
        capture: string
        case_id: string
        call_key: string
        case_hash: string
        egress_endpoint: string | null
      }>`
        WITH wanted AS (
          SELECT value AS capture FROM jsonb_array_elements_text(${wanted}::jsonb)
        ),
        standing AS (
          SELECT DISTINCT ON (c.capture) c.capture, c.prep_id
          FROM capture_cuts c JOIN wanted w ON w.capture = c.capture
          WHERE c.prep_id <= ${prepId}
          ORDER BY c.capture, c.prep_id DESC
        )
        SELECT cc.capture, cc.case_id, cc.call_key, cc.case_hash, cc.egress_endpoint
        FROM standing s
        JOIN cut_cases cc ON cc.prep_id = s.prep_id AND cc.capture = s.capture
        ORDER BY cc.capture, cc.case_id
      `
      for (const row of rows) {
        const list = found.get(row.capture) ?? []
        list.push({
          caseId: row.case_id,
          callKey: row.call_key,
          caseHash: row.case_hash,
          egressEndpoint: row.egress_endpoint
        })
        found.set(row.capture, list)
      }
      return found as ReadonlyMap<string, ReadonlyArray<CutCase>>
    })

    const standingPrep = Effect.gen(function*() {
      const rows = yield* sql<{ prep_id: string | null }>`
        SELECT max(prep_id) AS prep_id FROM capture_cuts
      `
      const id = rows[0]?.prep_id
      return id === null || id === undefined ? null : Number(id)
    })

    const latest = Effect.gen(function*() {
      const rows = yield* sql<{
        capture: string
        prep_id: string
        flows_hash: string | null
        cut_fingerprint: string | null
        broke: string | null
        cut_at: Date
      }>`
        SELECT DISTINCT ON (capture)
               capture, prep_id, flows_hash, cut_fingerprint, broke, cut_at
        FROM capture_cuts
        ORDER BY capture, prep_id DESC
      `
      const standing = new Map<string, CaptureCutState>()
      for (const row of rows) {
        standing.set(row.capture, {
          prepId: Number(row.prep_id),
          flowsHash: row.flows_hash,
          cutFingerprint: row.cut_fingerprint,
          broke: row.broke,
          cutAt: row.cut_at.toISOString()
        })
      }
      return standing as ReadonlyMap<string, CaptureCutState>
    })

    const captures = Effect.fn("Cuts.captures")(function*(prepId: number) {
      const rows = yield* sql<{ capture: string }>`
        SELECT capture FROM capture_cuts WHERE prep_id = ${prepId} ORDER BY capture
      `
      return rows.map((row) => row.capture)
    })

    const caseSpans = Effect.fn("Cuts.caseSpans")(function*(prepId?: number) {
      const rows = yield* sql<{ capture: string; case_id: string; span_ms: string | null }>`
        WITH standing AS (
          SELECT capture, max(prep_id) AS prep_id
          FROM capture_cuts
          WHERE ${prepId === undefined ? sql`true` : sql`prep_id <= ${prepId}`}
          GROUP BY capture
        )
        SELECT s.capture, c.case_id,
               (d.document->'timing'->>'capture_span_ms')::numeric::text AS span_ms
        FROM standing s
        JOIN cut_cases c ON c.prep_id = s.prep_id AND c.capture = s.capture
        JOIN cases cs ON cs.hash = c.case_hash
        JOIN case_documents d ON d.hash = cs.document_hash
        ORDER BY s.capture, c.case_id
      `
      const spans = new Map<string, Map<string, number | null>>()
      for (const row of rows) {
        let cases = spans.get(row.capture)
        if (cases === undefined) {
          cases = new Map()
          spans.set(row.capture, cases)
        }
        cases.set(row.case_id, row.span_ms === null ? null : Number(row.span_ms))
      }
      return spans as ReadonlyMap<string, ReadonlyMap<string, number | null>>
    })

    /**
     * The floor is applied per CAPTURE, not to the whole table: a capture cut
     * once, long before the floor, still stands at that old cut, so `>= floor`
     * alone would call its content dead. `standing` picks the newest cut no
     * later than the floor, and everything from there up is what any live
     * reading can name.
     */
    const contentFrom = Effect.fn("Cuts.contentFrom")(function*(floorPrep: number) {
      const rows = yield* sql<{ kind: string; value: string }>`
        WITH standing AS (
          SELECT capture, max(prep_id) AS prep_id
          FROM capture_cuts
          WHERE prep_id <= ${floorPrep}
          GROUP BY capture
        ),
        live AS (
          SELECT c.prep_id, c.capture, c.flows_hash
          FROM capture_cuts c
          LEFT JOIN standing s ON s.capture = c.capture
          WHERE c.prep_id >= coalesce(s.prep_id, 0)
        )
        SELECT 'case' AS kind, cc.case_hash AS value
        FROM cut_cases cc JOIN live l ON l.prep_id = cc.prep_id AND l.capture = cc.capture
        UNION
        SELECT 'flows' AS kind, l.flows_hash AS value
        FROM live l WHERE l.flows_hash IS NOT NULL
        UNION
        SELECT 'prep' AS kind, l.prep_id::text AS value
        FROM live l
      `
      const cases = new Set<string>()
      const flows = new Set<string>()
      const preps = new Set<number>()
      for (const row of rows) {
        if (row.kind === "case") cases.add(row.value)
        else if (row.kind === "flows") flows.add(row.value)
        else preps.add(Number(row.value))
      }
      return { cases, flows, preps } satisfies Live
    })

    const content = Effect.fn("Cuts.content")(function*(caseHash: string) {
      const documents = yield* sql<{ document: unknown }>`
        SELECT d.document
        FROM cases c JOIN case_documents d ON d.hash = c.document_hash
        WHERE c.hash = ${caseHash}
      `
      const document = documents[0]
      if (document === undefined) return null
      const assets = yield* sql<{ path: string; bytes: Uint8Array }>`
        SELECT path, bytes FROM case_assets WHERE case_hash = ${caseHash} ORDER BY path
      `
      const ledger = yield* sql<{ number: string; document: unknown }>`
        SELECT number, document FROM ledger_rows WHERE case_hash = ${caseHash} ORDER BY number
      `
      return {
        document: document.document,
        assets: assets.map((row) => ({ path: row.path, bytes: row.bytes } satisfies CaseAsset)),
        ledger: ledger.map((row) => ({ number: row.number, document: row.document } satisfies LedgerRow))
      } satisfies CaseContent
    })

    const flowsEnvelope = Effect.fn("Cuts.flowsEnvelope")(function*(flowsHash: string) {
      const rows = yield* sql<{ head: Uint8Array; tail: Uint8Array; legs: number }>`
        SELECT d.head, d.tail,
               (SELECT count(*)::int FROM flows_legs l WHERE l.flows_hash = d.hash) AS legs
        FROM flows_documents d
        WHERE d.hash = ${flowsHash}
      `
      const row = rows[0]
      return row === undefined
        ? null
        : { head: row.head, tail: row.tail, legs: row.legs } satisfies FlowsEnvelope
    })

    const flowsLegs = Effect.fn("Cuts.flowsLegs")(function*(
      flowsHash: string,
      from: number,
      limit: number
    ) {
      const rows = yield* sql<{ bytes: Uint8Array }>`
        SELECT bytes FROM flows_legs
        WHERE flows_hash = ${flowsHash} AND idx >= ${from}
        ORDER BY idx
        LIMIT ${limit}
      `
      return rows.map((row) => row.bytes)
    })

    const ledger = Effect.fn("Cuts.ledger")(function*(caseHashes: ReadonlyArray<string>) {
      if (caseHashes.length === 0) return []
      const rows = yield* sql<{ number: string; document: unknown }>`
        SELECT number, document FROM ledger_rows
        WHERE case_hash IN ${sql.in(caseHashes)}
        ORDER BY number
      `
      return rows.map((row) => ({ number: row.number, document: row.document } satisfies LedgerRow))
    })

    return Service.of({
      open,
      exists,
      finish,
      record,
      standing,
      resolve,
      cases,
      standingPrep,
      latest,
      captures,
      caseSpans,
      contentFrom,
      content,
      flowsEnvelope,
      flowsLegs,
      ledger
    })
  })
)
