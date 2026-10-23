/**
 * The REFUSAL record: the calls a cut made no case of, per (prep, capture),
 * beside the cases of the same reading.
 *
 * One row per CALL: the cut states one record per refusing verdict, and a
 * call several verdicts refused is one call refused under several reasons,
 * merged here before the write so no reason is lost to the last record. A
 * capture's refusals are replaced whole with its cut and never merged across
 * cuts, so a refusal the re-cut lifted does not stand; they go with the cut
 * they belong to, and the Call-IDs each names go with it as rows of their own
 * (`cut_refusal_calls`). `write` is the one writer, run inside the cut's own
 * transaction by `Cuts.record` and after the fact by the backfill; nothing
 * here rules on what a refusal means.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Refusal } from "./model.js"

/** A capture's standing cut, as far as refusals go: which prep, and how many rows it holds. */
export interface StandingCut {
  readonly capture: string
  readonly prepId: number
  readonly rows: number
}

export interface Interface {
  /** Records one capture's refusals under one prep, replacing what stood; answers the rows written. */
  readonly record: (
    prepId: number,
    capture: string,
    refusals: ReadonlyArray<Refusal>
  ) => Effect.Effect<number, SqlError>
  /**
   * Every capture's STANDING cut that did not break, with its refusal rows
   * counted, by capture. A broken cut refused nothing and has no record; a
   * record left beside it belongs to the reading before, never to it.
   */
  readonly standing: Effect.Effect<ReadonlyArray<StandingCut>, SqlError>
  /**
   * A cut's refusal of ONE call of a capture: the cut as of `prepId` (the
   * highest prep no later than it that cut the capture), the standing cut
   * where none is named; null where that cut made a case of it, never heard
   * of it, or broke.
   */
  readonly ofCase: (capture: string, caseId: string, prepId?: number) => Effect.Effect<Refusal | null, SqlError>
  /**
   * Why the cut as of `prepId` holds no case `caseId` of the capture, as one
   * token and one line: the cut refused it, another cut names it, the cut
   * broke, no cut of the capture stands as of the prep, or no cut names the id.
   */
  readonly whyNot: (capture: string, caseId: string, prepId: number) => Effect.Effect<Absence, SqlError>
}

/**
 * Why a case id names no case of a cut. The token is what a script matches
 * on; the reason is the line a reader acts on.
 */
export interface Absence {
  readonly caseId: string
  readonly token: "refused" | "not-in-this-cut" | "cut-broke" | "no-cut" | "unknown-id"
  readonly reason: string
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Refusals") {}

const distinct = (values: ReadonlyArray<string>): ReadonlyArray<string> => [...new Set(values)]

/** One refusal per call: reasons, Call-IDs and lines joined in record order, duplicates dropped. */
export const merge = (refusals: ReadonlyArray<Refusal>): ReadonlyArray<Refusal> => {
  const byCall = new Map<string, Refusal>()
  for (const refusal of refusals) {
    const held = byCall.get(refusal.caseId)
    const callKey = held?.callKey ?? refusal.callKey
    const replaces = held?.replaces ?? refusal.replaces
    byCall.set(
      refusal.caseId,
      held === undefined
        ? refusal
        : {
          caseId: refusal.caseId,
          reasons: distinct([...held.reasons, ...refusal.reasons]),
          callIds: distinct([...held.callIds, ...refusal.callIds]),
          detail: [held.detail, refusal.detail].filter((line) => line !== "").join("; "),
          ...(callKey === undefined ? {} : { callKey }),
          ...(replaces === undefined ? {} : { replaces })
        }
    )
  }
  return [...byCall.values()]
}

/** Replaces the (prep, capture)'s refusal rows in the caller's transaction; answers the rows written. */
export const write = (
  sql: SqlClient,
  prepId: number,
  capture: string,
  refusals: ReadonlyArray<Refusal>
): Effect.Effect<number, SqlError> =>
  Effect.gen(function*() {
    yield* sql`DELETE FROM cut_refusals WHERE prep_id = ${prepId} AND capture = ${capture}`
    let written = 0
    for (const refusal of merge(refusals)) {
      // The lists travel as JSON: a JS array is a value list to the statement
      // builder, not a postgres array.
      const rows = yield* sql<{ case_id: string }>`
        INSERT INTO cut_refusals (prep_id, capture, case_id, reasons, call_ids, detail, call_key)
        VALUES (${prepId}, ${capture}, ${refusal.caseId},
                (SELECT coalesce(array_agg(r), '{}'::text[])
                 FROM jsonb_array_elements_text(${JSON.stringify(refusal.reasons)}::jsonb) r),
                (SELECT coalesce(array_agg(id), '{}'::text[])
                 FROM jsonb_array_elements_text(${JSON.stringify(refusal.callIds)}::jsonb) id),
                ${refusal.detail}, ${refusal.callKey ?? null})
        RETURNING case_id
      `
      written += rows.length
      // The Call-IDs it names, one row each, for the lookup a call key makes.
      yield* sql`
        INSERT INTO cut_refusal_calls (prep_id, capture, call_id, case_id)
        SELECT DISTINCT ${prepId}::bigint, ${capture}::text, id, ${refusal.caseId}::text
        FROM jsonb_array_elements_text(${JSON.stringify(refusal.callIds)}::jsonb) id
      `
    }
    return written
  })

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const record = Effect.fn("Refusals.record")(function*(
      prepId: number,
      capture: string,
      refusals: ReadonlyArray<Refusal>
    ) {
      return yield* sql.withTransaction(write(sql, prepId, capture, refusals))
    })

    const standing = Effect.gen(function*() {
      const rows = yield* sql<{ capture: string; prep_id: string; rows: string }>`
        WITH standing AS (
          SELECT DISTINCT ON (capture) capture, prep_id, broke
          FROM capture_cuts
          ORDER BY capture, prep_id DESC
        )
        SELECT s.capture, s.prep_id,
               (SELECT count(*) FROM cut_refusals r WHERE r.prep_id = s.prep_id AND r.capture = s.capture) AS rows
        FROM standing s
        WHERE s.broke IS NULL
        ORDER BY s.capture
      `
      return rows.map((row) => ({
        capture: row.capture,
        prepId: Number(row.prep_id),
        rows: Number(row.rows)
      } satisfies StandingCut))
    })

    /** The capture's cut as of `prepId` (a ceiling), the standing one where none is named. */
    const cutAsOf = (capture: string, prepId: number | undefined) =>
      sql<{ prep_id: string; broke: string | null }>`
        SELECT prep_id, broke
        FROM capture_cuts
        WHERE capture = ${capture}
          AND ${prepId === undefined ? sql`true` : sql`prep_id <= ${prepId}`}
        ORDER BY prep_id DESC
        LIMIT 1
      `

    const ofCase = Effect.fn("Refusals.ofCase")(function*(capture: string, caseId: string, prepId?: number) {
      const cut = (yield* cutAsOf(capture, prepId))[0]
      if (cut === undefined || cut.broke !== null) return null
      const rows = yield* sql<{ reasons: ReadonlyArray<string>; call_ids: ReadonlyArray<string>; detail: string }>`
        SELECT reasons, call_ids, detail
        FROM cut_refusals
        WHERE prep_id = ${cut.prep_id} AND capture = ${capture} AND case_id = ${caseId}
      `
      const row = rows[0]
      return row === undefined
        ? null
        : { caseId, reasons: row.reasons, callIds: row.call_ids, detail: row.detail } satisfies Refusal
    })

    const whyNot = Effect.fn("Refusals.whyNot")(function*(capture: string, caseId: string, prepId: number) {
      const cut = (yield* cutAsOf(capture, prepId))[0]
      if (cut === undefined) {
        return { caseId, token: "no-cut", reason: `no cut of ${capture} stands as of prep${prepId}` } satisfies Absence
      }
      if (cut.broke !== null) {
        return { caseId, token: "cut-broke", reason: `prep${cut.prep_id} broke cutting ${capture}: ${cut.broke}` } satisfies
          Absence
      }
      const refusal = yield* ofCase(capture, caseId, prepId)
      if (refusal !== null) {
        return {
          caseId,
          token: "refused",
          reason: `prep${cut.prep_id} refused it: ${refusal.reasons.join(", ")}` +
            (refusal.detail === "" ? "" : ` — ${refusal.detail}`)
        } satisfies Absence
      }
      const elsewhere = yield* sql<{ prep_id: string; kind: string }>`
        SELECT prep_id, 'case' AS kind FROM cut_cases WHERE capture = ${capture} AND case_id = ${caseId}
        UNION ALL
        SELECT prep_id, 'refusal' FROM cut_refusals WHERE capture = ${capture} AND case_id = ${caseId}
        ORDER BY prep_id DESC
        LIMIT 1
      `
      const other = elsewhere[0]
      if (other !== undefined) {
        return {
          caseId,
          token: "not-in-this-cut",
          reason: `prep${other.prep_id} named it as a ${other.kind}; the cut it is read as of, ` +
            `prep${cut.prep_id}, does not name it`
        } satisfies Absence
      }
      return {
        caseId,
        token: "unknown-id",
        reason: `no cut of ${capture} ever named it: the resolver reads ${caseId} in every cited form`
      } satisfies Absence
    })

    return Service.of({ record, standing, ofCase, whyNot })
  })
)
