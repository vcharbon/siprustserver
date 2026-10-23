/**
 * The case documents a run's cells ran, by content hash, and the flows
 * document each capture's pinned cut confronted: what a reader needs to draw
 * one call's reference ladder beside its replay.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** A capture read as of a pinned prep. */
export interface Reading {
  readonly capture: string
  readonly prepId: number
}

export interface Interface {
  /** The documents by content hash; a hash the store holds none of is absent. */
  readonly documents: (hashes: ReadonlyArray<string>) => Effect.Effect<ReadonlyMap<string, unknown>, SqlError>
  /**
   * The flows document of each reading's cut — the capture's newest cut no
   * later than the pin — keyed by `readingKey`; a reading no cut answers is absent.
   */
  readonly flowsOf: (readings: ReadonlyArray<Reading>) => Effect.Effect<ReadonlyMap<string, string>, SqlError>
  /**
   * One field of each case's document, by case hash: the value at `field`
   * (a JSON path, key by key), so a reader of thousands of cases loads that
   * field alone. A case whose document holds no such field is absent.
   */
  readonly fieldOfCases: (
    caseHashes: ReadonlyArray<string>,
    field: ReadonlyArray<string>
  ) => Effect.Effect<ReadonlyMap<string, unknown>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/CaseDocuments") {}

/** The key `flowsOf` answers a reading under. */
export const readingKey = (capture: string, prepId: number): string => `${capture}\u0000${prepId}`

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const documents = Effect.fn("CaseDocuments.documents")(function*(hashes: ReadonlyArray<string>) {
      const found = new Map<string, unknown>()
      const wanted = [...new Set(hashes)]
      if (wanted.length === 0) return found
      const rows = yield* sql<{ hash: string; document: unknown }>`
        SELECT hash, document FROM case_documents WHERE hash IN ${sql.in(wanted)}
      `
      for (const row of rows) found.set(row.hash, row.document)
      return found
    })

    const flowsOf = Effect.fn("CaseDocuments.flowsOf")(function*(readings: ReadonlyArray<Reading>) {
      const found = new Map<string, string>()
      if (readings.length === 0) return found
      const rows = yield* sql<{ capture: string; pin: string; flows_hash: string | null }>`
        SELECT w.capture, w.pin,
               (SELECT c.flows_hash FROM capture_cuts c
                WHERE c.capture = w.capture AND c.prep_id <= w.pin
                ORDER BY c.prep_id DESC LIMIT 1) AS flows_hash
        FROM jsonb_to_recordset(${JSON.stringify(readings.map((one) => ({ capture: one.capture, pin: one.prepId })))}::jsonb)
          AS w(capture text, pin bigint)
      `
      for (const row of rows) {
        if (row.flows_hash !== null) found.set(readingKey(row.capture, Number(row.pin)), row.flows_hash)
      }
      return found
    })

    const fieldOfCases = Effect.fn("CaseDocuments.fieldOfCases")(function*(
      caseHashes: ReadonlyArray<string>,
      field: ReadonlyArray<string>
    ) {
      const found = new Map<string, unknown>()
      const wanted = [...new Set(caseHashes)]
      if (wanted.length === 0) return found
      const at = `{${field.map((key) => `"${key.replace(/["\\]/g, "\\$&")}"`).join(",")}}`
      const rows = yield* sql<{ hash: string; value: unknown }>`
        SELECT c.hash, d.document #> ${at}::text[] AS value
        FROM cases c JOIN case_documents d ON d.hash = c.document_hash
        WHERE c.hash IN ${sql.in(wanted)}
      `
      for (const row of rows) if (row.value !== null) found.set(row.hash, row.value)
      return found
    })

    return Service.of({ documents, flowsOf, fieldOfCases })
  })
)
