/**
 * The queue as CALLS: one row per current case of each capture on a lane,
 * plus one row per call the standing cut refused to make a case of, each with
 * its main error. Every capture is several independent questions, and this is
 * the record that keeps them apart.
 *
 * A case is shown at its CURRENT run — its latest `@current` run holding it
 * (the `current_cells` view): the rule `Runs.standings` reads by.
 * A capture no `@current` run holds has no case rows. The standing cut says
 * what stands cut: a call it refused is one refused row, and a case row of the
 * same call from an older reading is not served beside it.
 *
 * `queue` is ONE statement: the scope, the capture substring and the chips
 * narrow in SQL, the count and the chip groups come back beside the page, and
 * the page is ordered and limited before its call keys are probed.
 *
 * `hydrate` reads the documents of the rows actually served. Both read; the
 * error columns are written by the run's own write and by `measure.ts`.
 *
 * Nothing here holds judgment: a row says what the run and the cut recorded.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Fragment } from "effect/unstable/sql/Statement"
import { type Summary, summaryOf, EMPTY } from "./call-summary.js"
import { DEFAULT_SCOPE, type Group, type Outcome, type Query } from "./facets.js"
import { MainError as MainErrors } from "@sip/contracts"
import type { MainError, MainErrorKind, SetKind } from "./model.js"
import * as Sessions from "./sessions.js"

export type RowKind = "case" | "refused"

/** One call of the lane's queue: a current case, or a call the standing cut refused. */
export interface CallRow {
  readonly capture: string
  readonly caseId: string
  readonly kind: RowKind
  /**
   * The call key the row's call is named by in a request cell: a case's from
   * the newest cut naming its id, a refused call's the root of the Call-IDs the
   * refusal names (the shortest, as the cut's own rule picks it). Null where no
   * cut of the store names the case.
   */
  readonly callKey: string | null
  /** The case's current run; null for a refused call, which no run holds. */
  readonly runId: number | null
  /** The set kind of that run. */
  readonly setKind: SetKind | null
  /** The standing cut a refused call belongs to; null for a case. */
  readonly prepId: number | null
  readonly bundlePath: string | null
  /** The v3 document the case ran, by content hash; what `hydrate` joins on. */
  readonly documentHash: string | null
  readonly passed: boolean
  readonly skipped: string | null
  readonly failedStep: string | null
  readonly rfcGating: number | null
  /** Every reason the cut refused the call under, the first leading; empty for a case. */
  readonly reasons: ReadonlyArray<string>
  /** The features of the document the case ran (`case_documents.features`); empty for a refused call. */
  readonly features: ReadonlyArray<string>
  /** What the call failed on; null where it passed, was skipped, or was never measured. */
  readonly mainError: MainError | null
  /** The body rows no rule accepts, whatever `mainError` names; absent reads as 0. */
  readonly bodyFailing?: number
  /** When the run finished, or when the refusing cut was taken. */
  readonly finishedAt: string | null
}

/** A served row with what its document says. */
export interface CallDetail extends CallRow, Summary {}

/** What one query of a lane answers. */
export interface Queue {
  /** The page: the first `limit` rows the query keeps, cases first, then by capture and case id. */
  readonly rows: ReadonlyArray<CallRow>
  /** Rows the query keeps, before the limit. */
  readonly matched: number
  /** Distinct captures among the rows the scope, the capture substring and the excludes keep. */
  readonly captures: number
  /** The pool's groups — the scope's and the capture's rows, whatever the chips — with their counts. */
  readonly groups: ReadonlyArray<Group>
}

export interface Interface {
  /** The lane under a query, as one statement answers it; `limit` 0 serves every kept row. */
  readonly queue: (lane: string, query: Query, limit: number) => Effect.Effect<Queue, SqlError>
  /** The plan of the statement `queue` runs, line by line, under `EXPLAIN (ANALYZE, BUFFERS)`. */
  readonly explain: (lane: string, query: Query, limit: number) => Effect.Effect<ReadonlyArray<string>, SqlError>
  /** The rows with their documents' facts; a row with no document hydrates to nulls. Order kept. */
  readonly hydrate: (rows: ReadonlyArray<CallRow>) => Effect.Effect<ReadonlyArray<CallDetail>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Calls") {}

/** A served row as the statement builds it, one shape for a case and a refused call. */
interface ServedRow {
  readonly kind: RowKind
  readonly capture: string
  readonly case_id: string
  readonly call_key: string | null
  readonly run_id: number | null
  readonly set_kind: SetKind | null
  readonly prep_id: number | null
  readonly bundle_path: string | null
  readonly finished_at: string | null
  readonly document_hash: string | null
  readonly passed: boolean
  readonly skipped: string | null
  readonly failed_step: string | null
  readonly rfc_gating: number | null
  readonly reasons: ReadonlyArray<string>
  readonly features: ReadonlyArray<string>
  readonly error_kind: string | null
  readonly error_rule: string | null
  readonly error_rfc: string | null
  readonly error_step: string | null
  readonly error_expected: string | null
  readonly error_detail: string | null
  readonly body_failing: number
}

interface GroupRow {
  readonly kind: RowKind
  readonly outcome: Outcome
  readonly error_kind: string | null
  readonly rule: string | null
  readonly rfc: string | null
  readonly expected: string | null
  readonly reasons: ReadonlyArray<string>
  readonly features: ReadonlyArray<string>
  readonly bad_body: boolean
  readonly count: number
}

interface QueueRow {
  readonly matched: number
  readonly captures: number
  readonly groups: ReadonlyArray<GroupRow>
  readonly rows: ReadonlyArray<ServedRow>
}

const errorOf = (row: ServedRow): MainError | null =>
  row.kind === "refused"
    ? {
      kind: "refused",
      rule: row.reasons[0] ?? null,
      rfc: null,
      step: null,
      expected: null,
      detail: row.error_detail ?? ""
    }
    : row.error_kind === null
    ? null
    : {
      kind: row.error_kind as MainErrorKind,
      rule: row.error_rule,
      rfc: row.error_rfc,
      step: row.error_step,
      expected: row.error_expected,
      detail: row.error_detail ?? ""
    }

const blank = (value: string | undefined): value is undefined => value === undefined || value === ""

/** The substring as a LIKE pattern reading it literally: `\`, `%` and `_` are characters. */
const patternOf = (substring: string): string => `%${substring.replace(/[\\%_]/g, "\\$&")}%`

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    /** A text list as a postgres array: a JS array is a value list to the statement builder. */
    const textArray = (values: ReadonlyArray<string>): Fragment =>
      sql`ARRAY(SELECT jsonb_array_elements_text(${JSON.stringify(values)}::jsonb))`

    const statement = (lane: string, query: Query, limit: number) => {
      const scope = query.scope ?? DEFAULT_SCOPE
      const captures = blank(query.capture) ? sql`true` : sql`capture ILIKE ${patternOf(query.capture)}`
      const inScope = scope === "all"
        ? sql`true`
        : scope === "red"
        ? sql`outcome IN ('failed', 'refused')`
        : sql`outcome = ${scope}`
      const exclude = query.exclude ?? {}
      /** The rows a chip group's excludes keep: none of the values, a null answering to none. */
      const notIn = (column: Fragment, values: ReadonlyArray<string> | undefined): Fragment =>
        values === undefined || values.length === 0
          ? sql`true`
          : sql`NOT coalesce(${column} = ANY(${textArray(values)}), false)`
      const notHidden = sql.and([
        notIn(sql`rule`, exclude.rule),
        exclude.rule === undefined || !exclude.rule.includes(MainErrors.BAD_BODY) ? sql`true` : sql`NOT bad_body`,
        notIn(sql`expected`, exclude.expected),
        notIn(sql`outcome`, exclude.outcome),
        exclude.refusal === undefined || exclude.refusal.length === 0
          ? sql`true`
          : sql`NOT (reasons && ${textArray(exclude.refusal)})`,
        exclude.feature === undefined || exclude.feature.length === 0
          ? sql`true`
          : sql`NOT (features && ${textArray(exclude.feature)})`
      ])
      const included = sql.and([
        blank(query.rule)
          ? sql`true`
          : query.rule === MainErrors.BAD_BODY
          ? sql`(rule = ${query.rule} OR bad_body)`
          : sql`rule = ${query.rule}`,
        blank(query.expected) ? sql`true` : sql`expected = ${query.expected}`,
        blank(query.refusal) ? sql`true` : sql`${query.refusal} = ANY(reasons)`,
        blank(query.outcome) ? sql`true` : sql`outcome = ${query.outcome}`,
        blank(query.feature) ? sql`true` : sql`${query.feature} = ANY(features)`
      ])
      return sql<QueueRow>`
        WITH known AS (
          SELECT capture FROM population WHERE ${captures}
          UNION
          SELECT DISTINCT capture FROM current_runs WHERE lane = ${lane} AND ${captures}
        ),
        standing_cut AS (
          SELECT DISTINCT ON (c.capture) c.capture, c.prep_id, c.cut_at
          FROM capture_cuts c
          JOIN known k ON k.capture = c.capture
          ORDER BY c.capture, c.prep_id DESC
        ),
        refused AS (
          SELECT r.capture, r.case_id, r.prep_id, r.reasons, r.detail, s.cut_at,
                 (SELECT id FROM unnest(r.call_ids) AS id ORDER BY length(id), id LIMIT 1) AS call_key
          FROM cut_refusals r
          JOIN standing_cut s ON s.prep_id = r.prep_id AND s.capture = r.capture
        ),
        cases AS (
          SELECT c.capture, c.case_id, c.run_id, c.set_kind, c.bundle_path, c.finished_at,
                 c.document_hash, c.passed, c.skipped, c.failed_step, c.rfc_gating,
                 c.error_kind, c.error_rule, c.error_rfc, c.error_step, c.error_expected, c.error_detail,
                 c.body_failing
          FROM current_cells c
          WHERE c.lane = ${lane} AND ${captures}
            AND NOT EXISTS (SELECT 1 FROM refused x WHERE x.capture = c.capture AND x.case_id = c.case_id)
        ),
        -- One shape for a case and a refused call; a case's outcome is skipped
        -- before passed, as the outcome chips read it. A case's features are its
        -- document's stored column, one primary-key probe per case.
        pool AS (
          SELECT * FROM (
            SELECT 'case'::text AS kind, c.capture, c.case_id,
                   CASE WHEN c.skipped IS NOT NULL THEN 'skipped' WHEN c.passed THEN 'passed' ELSE 'failed' END AS outcome,
                   c.error_kind, c.error_rule AS rule, c.error_rfc AS rfc, c.error_expected AS expected,
                   '{}'::text[] AS reasons, coalesce(d.features, '{}'::text[]) AS features, NULL::text AS call_key,
                   c.run_id, c.set_kind, NULL::bigint AS prep_id, c.bundle_path, c.finished_at, c.document_hash,
                   c.passed, c.skipped, c.failed_step, c.rfc_gating, c.error_step, c.error_detail,
                   c.body_failing, c.body_failing > 0 AS bad_body
            FROM cases c
            LEFT JOIN case_documents d ON d.hash = c.document_hash
            UNION ALL
            SELECT 'refused', capture, case_id, 'refused',
                   'refused', NULL, NULL, NULL,
                   reasons, '{}'::text[], call_key,
                   NULL, NULL, prep_id, NULL, cut_at, NULL,
                   false, NULL, NULL, NULL, NULL, detail,
                   0, false
            FROM refused
          ) u
          WHERE ${inScope}
        ),
        kept AS (
          SELECT * FROM pool WHERE ${notHidden}
        ),
        matched AS (
          SELECT * FROM kept WHERE ${included}
        ),
        page AS (
          SELECT * FROM matched ORDER BY kind, capture COLLATE "C", case_id COLLATE "C" LIMIT ${limit > 0 ? limit : null}
        ),
        served AS (
          SELECT p.*, CASE WHEN p.kind = 'refused' THEN p.call_key ELSE k.call_key END AS key
          FROM page p
          LEFT JOIN LATERAL (
            SELECT cc.call_key FROM cut_cases cc
            WHERE cc.capture = p.capture AND cc.case_id = p.case_id
            ORDER BY cc.prep_id DESC
            LIMIT 1
          ) k ON true
        )
        SELECT
          (SELECT count(*)::int FROM matched) AS matched,
          (SELECT count(DISTINCT capture)::int FROM kept) AS captures,
          (SELECT coalesce(jsonb_agg(jsonb_build_object(
             'kind', g.kind, 'outcome', g.outcome, 'error_kind', g.error_kind, 'rule', g.rule,
             'rfc', g.rfc, 'expected', g.expected, 'reasons', to_jsonb(g.reasons),
             'features', to_jsonb(g.features), 'bad_body', g.bad_body, 'count', g.count
           )), '[]'::jsonb)
           FROM (
             SELECT kind, outcome, error_kind, rule, rfc, expected, reasons, features, bad_body, count(*)::int AS count
             FROM pool
             GROUP BY kind, outcome, error_kind, rule, rfc, expected, reasons, features, bad_body
           ) g) AS groups,
          (SELECT coalesce(jsonb_agg(jsonb_build_object(
             'kind', s.kind, 'capture', s.capture, 'case_id', s.case_id, 'call_key', s.key,
             'run_id', s.run_id, 'set_kind', s.set_kind, 'prep_id', s.prep_id, 'bundle_path', s.bundle_path,
             'finished_at', s.finished_at, 'document_hash', s.document_hash, 'passed', s.passed,
             'skipped', s.skipped, 'failed_step', s.failed_step, 'rfc_gating', s.rfc_gating,
             'reasons', to_jsonb(s.reasons), 'features', to_jsonb(s.features), 'error_kind', s.error_kind, 'error_rule', s.rule,
             'error_rfc', s.rfc, 'error_step', s.error_step, 'error_expected', s.expected,
             'error_detail', s.error_detail, 'body_failing', s.body_failing
           ) ORDER BY s.kind, s.capture COLLATE "C", s.case_id COLLATE "C"), '[]'::jsonb)
           FROM served s) AS rows
      `
    }

    const rowOf = (row: ServedRow): CallRow => ({
      capture: row.capture,
      caseId: row.case_id,
      kind: row.kind,
      callKey: row.call_key,
      runId: row.run_id === null ? null : Number(row.run_id),
      setKind: row.set_kind,
      prepId: row.prep_id === null ? null : Number(row.prep_id),
      bundlePath: row.bundle_path === null ? null : Sessions.fromStored(root, row.bundle_path),
      documentHash: row.document_hash,
      passed: row.passed,
      skipped: row.skipped,
      failedStep: row.failed_step,
      rfcGating: row.rfc_gating === null ? null : Number(row.rfc_gating),
      reasons: row.reasons,
      features: row.features,
      mainError: errorOf(row),
      bodyFailing: Number(row.body_failing),
      finishedAt: row.finished_at === null ? null : new Date(row.finished_at).toISOString()
    })

    const groupOf = (row: GroupRow): Group => ({
      kind: row.kind,
      outcome: row.outcome,
      errorKind: row.error_kind,
      rule: row.rule,
      rfc: row.rfc,
      expected: row.expected,
      reasons: row.reasons,
      features: row.features,
      ...(row.bad_body ? { badBody: true as const } : {}),
      count: Number(row.count)
    })

    const queue = Effect.fn("Calls.queue")(function*(lane: string, query: Query, limit: number) {
      const answer = (yield* statement(lane, query, limit))[0]
      return answer === undefined
        ? { rows: [], matched: 0, captures: 0, groups: [] } satisfies Queue
        : {
          rows: answer.rows.map(rowOf),
          matched: Number(answer.matched),
          captures: Number(answer.captures),
          groups: answer.groups.map(groupOf)
        } satisfies Queue
    })

    // The statement `queue` runs, compiled as the client sends it, under the planner.
    const explain = Effect.fn("Calls.explain")(function*(lane: string, query: Query, limit: number) {
      const [text, params] = statement(lane, query, limit).compile()
      const rows = yield* sql.unsafe<{ "QUERY PLAN": string }>(`EXPLAIN (ANALYZE, BUFFERS) ${text}`, params)
      return rows.map((row) => row["QUERY PLAN"])
    })

    const hydrate = Effect.fn("Calls.hydrate")(function*(rows: ReadonlyArray<CallRow>) {
      const hashes = [...new Set(rows.flatMap((row) => (row.documentHash === null ? [] : [row.documentHash])))]
      const documents = new Map<string, Summary>()
      if (hashes.length > 0) {
        const found = yield* sql<{ hash: string; document: unknown }>`
          SELECT hash, document FROM case_documents WHERE hash IN ${sql.in(hashes)}
        `
        for (const row of found) documents.set(row.hash, summaryOf(row.document))
      }
      return rows.map((row): CallDetail => ({
        ...row,
        ...(row.documentHash === null ? EMPTY : documents.get(row.documentHash) ?? EMPTY)
      }))
    })

    return Service.of({ queue, explain, hydrate })
  })
)
