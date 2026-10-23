/**
 * The CASE-ID RESOLVER: every form a ticket cites a call by,
 * read back to the call the store holds under it.
 *
 * Forms, tried in this order:
 *   runN/caseN, runN/<case id>    a case of the capture run N replayed, read in
 *                                 the cut run N pinned; the answer names the
 *                                 run's start, capture and prep
 *   runN, N                       a run, which is no call: its capture is named
 *   caseN, cutN, -caseN, -auto    a bare suffix: every capture's case of that name
 *   <capture>…-auto               an elided id (`…` or `...`): the ids it can stand for
 *   <capture>-<suffix>            a full case id, the capture by its name or its stem
 *   <capture>, <capture stem>     a capture ({@link Grammar}), which is no call: its cases are counted
 *   anything else                 a Call-ID, or a call key
 *
 * A case id is looked up over every reading that named it — the cut rows a
 * prep still holds (`cut_cases`, `cut_refusals`) and the case map every prep
 * leaves (`case_maps`) — and the frozen ids (`case_ids`): the newest prep
 * naming it answers, a refusal there answers `refused` with its reasons, a
 * call the capture's standing cut no longer names says so, and an id only the
 * frozen ids hold answers with its call and no prep. A run whose row is gone
 * answers from its cell summaries where a citation kept them. The store was
 * rebuilt on the date its first prep was cut, and every id then restarted: a
 * run id the store never issued (above the highest it issued, or skipped when
 * the wipe raised the sequence), or an id the caller says was cited before
 * that date, answers `pre-rebuild`.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Fragment } from "effect/unstable/sql/Statement"

/** How an input was read. */
export type Form = "run-case" | "run" | "bare-case" | "pattern" | "case-id" | "capture" | "call-id"

/** What the input came to. */
export type Status =
  | "resolved"
  | "refused"
  | "ambiguous"
  | "not-found"
  | "pre-rebuild"
  | "a-run"
  | "a-capture"

/** One call the input names, as the newest reading naming it reads it. */
export interface Call {
  readonly capture: string
  readonly caseId: string
  /** The capture's own name for the call; null where only a run, no reading, names it. */
  readonly callKey: string | null
  /** The newest prep naming the case; null where only the frozen ids or a run do. */
  readonly prepId: number | null
  /** `refused` where that prep refused the call rather than cut it. */
  readonly kind: "case" | "refused"
  /** The refusal reasons, first leading; empty for a case. */
  readonly reasons: ReadonlyArray<string>
  /** Whether the capture's standing cut (its newest prep) still names the call. */
  readonly standing: boolean
}

export interface Resolution {
  readonly input: string
  readonly form: Form
  readonly status: Status
  /** The one call, or the candidates of an ambiguous input (at most `limit`). */
  readonly calls: ReadonlyArray<Call>
  /** How many calls the input could stand for, beyond the `calls` listed. */
  readonly total: number
  /** One line saying what the input is. */
  readonly note: string
}

export interface Options {
  /** When the citing ticket wrote the id (`YYYY-MM-DD`); before the rebuild it names a call the store no longer knows. */
  readonly citedOn?: string
  /** Candidates listed for an ambiguous input. */
  readonly limit?: number
}

export interface Interface {
  readonly resolve: (input: string, options?: Options) => Effect.Effect<Resolution, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Resolver") {}

/** The forms, as one line a refusal can quote. */
export const FORMS = "runN/caseN, <capture>-<case suffix>, caseN, <capture>…-auto, a capture, a Call-ID"

const RUN = /^(?:@run:|run)?(\d+)$/
const RUN_CASE = /^(?:@run:|run)(\d+)\/(.+)$/
const ELISION = /…|\.\.\./

/**
 * The deployment's grammar: which inputs name a CAPTURE, and which suffixes
 * end a case id (`<capture>-<suffix>`). How a capture is named and how the
 * deployment names a call of it are its corpus conventions.
 */
export interface Grammar {
  /** Whether an input that is no case id names a capture, by its file name or its stem. */
  readonly isCapture: (input: string) => boolean
  /** The case-id suffixes, as the alternatives of a regular expression; {@link DEFAULT_SUFFIXES} where absent. */
  readonly suffixes?: string
  /**
   * The suffixes a prose text cites a case by ALONE — none a word of prose
   * reads as (`auto` does) — as alternatives; {@link DEFAULT_BARE_SUFFIXES}
   * where absent.
   */
  readonly bareSuffixes?: string
}

/** The suffixes a grammar that states none reads: `caseN`, `cutN`, `auto`, `legsA+B`. */
export const DEFAULT_SUFFIXES = String.raw`case\d+|cut\d+|auto|legs[\d+]+`

/** The suffixes a text cites a case by alone, where a grammar states none: `caseN`. */
export const DEFAULT_BARE_SUFFIXES = String.raw`case\d+`

/** A capture is a packet-capture file: its name carries `.pcap` (`.pcapng` included). */
export const DEFAULT_GRAMMAR: Grammar = { isCapture: (input) => input.includes(".pcap") }

/**
 * The case-id shapes a grammar reads — the one source every reader of a case
 * id builds on (the resolver, the id forms, the citation scan, the bundle
 * import): its suffixes and the anchored shapes of a suffix, a full case id
 * (`<capture>-<suffix>`, the capture captured) and a bare suffix (`-` optional).
 */
export interface CaseShapes {
  readonly suffixes: string
  readonly bareSuffixes: string
  readonly suffix: RegExp
  readonly caseId: RegExp
  readonly bare: RegExp
}

export const caseShapes = (grammar: Grammar): CaseShapes => {
  const suffixes = grammar.suffixes ?? DEFAULT_SUFFIXES
  return {
    suffixes,
    bareSuffixes: grammar.bareSuffixes ?? DEFAULT_BARE_SUFFIXES,
    suffix: new RegExp(`^(${suffixes})$`),
    caseId: new RegExp(`^(.+)-(${suffixes})$`),
    bare: new RegExp(`^-?(${suffixes})$`)
  }
}

const shapesOf = caseShapes

/** How an input reads, before any lookup. */
export const formOf = (input: string, grammar: Grammar = DEFAULT_GRAMMAR): Form => {
  const shapes = shapesOf(grammar)
  if (RUN_CASE.test(input)) return "run-case"
  if (RUN.test(input)) return "run"
  if (shapes.bare.test(input)) return "bare-case"
  if (ELISION.test(input)) return "pattern"
  if (shapes.caseId.test(input)) return "case-id"
  return grammar.isCapture(input) ? "capture" : "call-id"
}

/** A LIKE pattern reading every character literally. */
const literal = (text: string): string => text.replace(/[\\%_]/g, "\\$&")

interface CallRow {
  readonly capture: string
  readonly case_id: string
  readonly call_key: string | null
  readonly prep_id: string | null
  readonly kind: "case" | "refused"
  readonly reasons: ReadonlyArray<string> | null
  readonly standing: boolean
  readonly total: string
}

const callOf = (row: CallRow): Call => ({
  capture: row.capture,
  caseId: row.case_id,
  callKey: row.call_key,
  prepId: row.prep_id === null ? null : Number(row.prep_id),
  kind: row.kind,
  reasons: row.reasons ?? [],
  standing: row.standing === true
})

/** The resolver reading capture names by `grammar`. */
export const layerWith = (grammar: Grammar) => Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const shapes = shapesOf(grammar)

    /** When the store's ids began: its first prep's cut, as `YYYY-MM-DD`; null for a store that never cut. */
    const rebuiltOn = Effect.gen(function*() {
      const rows = yield* sql<{ at: Date | null }>`
        SELECT least((SELECT min(cut_at) FROM capture_cuts), (SELECT min(cut_at) FROM case_maps)) AS at
      `
      const at = rows[0]?.at ?? null
      return at === null ? null : at.toISOString().slice(0, 10)
    })

    /** The smallest Call-ID of a refusal: the call it names where it recorded no key. */
    const keyOfRefusal = (alias: string) =>
      sql.literal(
        `coalesce(${alias}.call_key, (SELECT id FROM unnest(${alias}.call_ids) AS id ORDER BY length(id), id LIMIT 1))`
      )

    /**
     * Every reading naming a case id `matches` admits — cut rows, case maps,
     * then the frozen ids — as (capture, case_id, call_key, prep_id, kind,
     * reasons, rank): the rank orders sources naming the same prep.
     */
    const named = (matches: (column: Fragment) => Fragment) => sql`
      SELECT capture, case_id, call_key, prep_id, 'case'::text AS kind, NULL::text[] AS reasons, 0 AS rank
      FROM cut_cases WHERE ${matches(sql.literal("case_id"))}
      UNION ALL
      SELECT r.capture, r.case_id, ${keyOfRefusal("r")}, r.prep_id, 'refused', r.reasons, 0
      FROM cut_refusals r WHERE ${matches(sql.literal("r.case_id"))}
      UNION ALL
      SELECT m.capture, m.case_id,
             CASE WHEN m.kind = 'case' THEN m.call_key ELSE ${keyOfRefusal("m")} END,
             m.prep_id, m.kind, CASE WHEN m.kind = 'case' THEN NULL ELSE m.reasons END, 1
      FROM case_maps m WHERE ${matches(sql.literal("m.case_id"))}
      UNION ALL
      SELECT capture, case_id, call_key, NULL::bigint, 'case', NULL::text[], 2
      FROM case_ids WHERE ${matches(sql.literal("case_id"))}
    `

    /** The newest reading of each (capture, case id) among `rows`, with the standing flag and the total. */
    const newestOf = (rows: Fragment, limit: number) =>
      sql<CallRow>`
        WITH named AS (${rows}),
        newest AS (
          SELECT DISTINCT ON (capture, case_id) * FROM named
          ORDER BY capture, case_id, prep_id DESC NULLS LAST, rank
        )
        SELECT n.capture, n.case_id, n.call_key, n.prep_id, n.kind, n.reasons,
               n.prep_id IS NOT NULL
                 AND n.prep_id = (SELECT max(c.prep_id) FROM capture_cuts c WHERE c.capture = n.capture) AS standing,
               count(*) OVER () AS total
        FROM newest n
        ORDER BY n.capture, n.case_id
        LIMIT ${limit}
      `

    const callsWhere = (matches: (column: Fragment) => Fragment, limit: number) =>
      newestOf(named(matches), limit)

    /**
     * The call a capture's case id named in the cut a run pinned: the
     * capture's newest reading no later than the pin, its case or its
     * refusal, off the cut rows or the case map.
     */
    const callAsOf = (capture: string, caseId: string, pin: number) =>
      sql<CallRow>`
        WITH cut AS (
          SELECT max(prep_id) AS prep_id FROM (
            SELECT prep_id FROM capture_cuts WHERE capture = ${capture} AND prep_id <= ${pin}
            UNION ALL
            SELECT prep_id FROM case_maps WHERE capture = ${capture} AND prep_id <= ${pin}
          ) readings
        ),
        named AS (
          SELECT c.capture, c.case_id, c.call_key, c.prep_id, 'case'::text AS kind, NULL::text[] AS reasons, 0 AS rank
          FROM cut_cases c JOIN cut ON cut.prep_id = c.prep_id
          WHERE c.capture = ${capture} AND c.case_id = ${caseId}
          UNION ALL
          SELECT r.capture, r.case_id, ${keyOfRefusal("r")}, r.prep_id, 'refused', r.reasons, 0
          FROM cut_refusals r JOIN cut ON cut.prep_id = r.prep_id
          WHERE r.capture = ${capture} AND r.case_id = ${caseId}
          UNION ALL
          SELECT m.capture, m.case_id,
                 CASE WHEN m.kind = 'case' THEN m.call_key ELSE ${keyOfRefusal("m")} END,
                 m.prep_id, m.kind, CASE WHEN m.kind = 'case' THEN NULL ELSE m.reasons END, 1
          FROM case_maps m JOIN cut ON cut.prep_id = m.prep_id
          WHERE m.capture = ${capture} AND m.case_id = ${caseId}
        ),
        newest AS (SELECT DISTINCT ON (capture, case_id) * FROM named ORDER BY capture, case_id, rank)
        SELECT n.capture, n.case_id, n.call_key, n.prep_id, n.kind, n.reasons,
               n.prep_id = (SELECT max(c.prep_id) FROM capture_cuts c WHERE c.capture = n.capture) AS standing,
               count(*) OVER () AS total
        FROM newest n
      `

    /** A case id no reading names but a run replayed: the run, or its summary, is the only witness. */
    const ranOnly = (caseId: string) =>
      sql<{ capture: string; call_key: string | null }>`
        SELECT capture, call_key FROM (
          SELECT r.capture, NULL::text AS call_key, r.id FROM case_results cr JOIN runs r ON r.id = cr.run_id
          WHERE cr.case_id = ${caseId}
          UNION ALL
          SELECT capture, call_key, run_id FROM cell_summaries WHERE case_id = ${caseId}
        ) ran
        ORDER BY id DESC LIMIT 1
      `

    const answer = (
      input: string,
      form: Form,
      rows: ReadonlyArray<CallRow>,
      what: string
    ): Resolution => {
      const calls = rows.map(callOf)
      const total = rows.length === 0 ? 0 : Number(rows[0]!.total)
      if (calls.length === 0) {
        return { input, form, status: "not-found", calls, total, note: `no cut names ${what} (forms: ${FORMS})` }
      }
      if (total > 1) {
        return {
          input,
          form,
          status: "ambiguous",
          calls,
          total,
          note: `${total} calls are named ${what}: name the capture (${FORMS})`
        }
      }
      const only = calls[0]!
      const where = only.prepId === null || only.standing
        ? ""
        : ` (not in the capture's standing cut; last named by prep${only.prepId})`
      return only.kind === "refused"
        ? {
          input,
          form,
          status: "refused",
          calls,
          total,
          note: `refused by prep${only.prepId}: ${only.reasons.join(", ")}${where}`
        }
        : {
          input,
          form,
          status: "resolved",
          calls,
          total,
          note: only.prepId === null ? "a frozen id: no reading the store holds names it" : `prep${only.prepId}${where}`
        }
    }

    const byCaseId = Effect.fn("Resolver.byCaseId")(function*(input: string, form: Form, caseId: string) {
      const rows = yield* callsWhere((column) => sql`${column} = ${caseId}`, 2)
      if (rows.length > 0) return answer(input, form, rows, caseId)
      // A capture cited by its stem: `trace_205845-case1` for `trace_205845.pcap.gz-case1`.
      const parts = shapes.caseId.exec(caseId)
      if (parts !== null && !parts[1]!.includes(".")) {
        const stemmed = yield* callsWhere(
          (column) => sql`${column} LIKE ${`${literal(parts[1]!)}.%-${literal(parts[2]!)}`}`,
          20
        )
        if (stemmed.length > 0) return answer(input, form, stemmed, caseId)
      }
      const ran = yield* ranOnly(caseId)
      if (ran.length > 0) {
        return {
          input,
          form,
          status: "resolved",
          calls: [{
            capture: ran[0]!.capture,
            caseId,
            callKey: ran[0]!.call_key,
            prepId: null,
            kind: "case",
            reasons: [],
            standing: false
          }],
          total: 1,
          note: "no cut of the store names it; a run replayed it"
        } satisfies Resolution
      }
      return answer(input, form, [], caseId)
    })

    /**
     * Whether the store never issued run `id`: it is above the highest run id
     * issued, or among those a raised sequence skipped (`id_gaps`). A run cited
     * so is another store's.
     */
    const neverIssued = (id: number) =>
      Effect.gen(function*() {
        const rows = yield* sql<{ highest: string | null; skipped: boolean }>`
          SELECT greatest(
                   (SELECT max(id) FROM runs),
                   (SELECT max(run_id) FROM run_summaries),
                   (SELECT CASE WHEN is_called THEN last_value END FROM runs_id_seq)
                 ) AS highest,
                 EXISTS (
                   SELECT 1 FROM id_gaps WHERE sequence = 'runs_id_seq' AND ${id} >= first AND ${id} < next
                 ) AS skipped
        `
        const highest = Number(rows[0]?.highest ?? 0)
        return { highest, never: id > highest || rows[0]?.skipped === true }
      })

    /** A run whose row is gone, read off the summaries a citation kept of its cells. */
    const summarisedRun = Effect.fn("Resolver.summarisedRun")(function*(
      input: string,
      form: Form,
      id: number,
      named: string | undefined
    ) {
      const head = yield* sql<{
        capture: string
        lane: string
        env: string | null
        status: string
        started_at: Date
        cells: string
      }>`
        SELECT s.capture, s.lane, s.env, s.status, s.started_at,
               (SELECT count(*) FROM cell_summaries c WHERE c.run_id = s.run_id) AS cells
        FROM run_summaries s WHERE s.run_id = ${id}
      `
      const run = head[0]
      if (run === undefined) return undefined
      const said = `run ${id} (started ${run.started_at.toISOString()}, ${run.capture}; its row is gone, ` +
        `${run.cells} cell summar${Number(run.cells) === 1 ? "y" : "ies"} kept)`
      if (named === undefined) {
        return {
          input,
          form,
          status: "a-run",
          calls: [],
          total: 0,
          note: `${said} is a run, not a call: on ${run.lane}, env ${run.env ?? "unknown"}, ${run.status}`
        } satisfies Resolution
      }
      const caseId = shapes.suffix.test(named) ? `${run.capture}-${named}` : named
      const cells = yield* sql<{ call_key: string | null; verdict: string }>`
        SELECT call_key, verdict FROM cell_summaries WHERE run_id = ${id} AND case_id = ${caseId}
      `
      const cell = cells[0]
      if (cell === undefined) {
        return { input, form, status: "not-found", calls: [], total: 0, note: `${said} holds no case ${caseId}` } satisfies
          Resolution
      }
      return {
        input,
        form,
        status: "resolved",
        calls: [{
          capture: run.capture,
          caseId,
          callKey: cell.call_key,
          prepId: null,
          kind: "case",
          reasons: [],
          standing: false
        }],
        total: 1,
        note: `${said}: ${cell.verdict}`
      } satisfies Resolution
    })

    const resolve = Effect.fn("Resolver.resolve")(function*(raw: string, options: Options = {}) {
      const input = raw.trim()
      const limit = options.limit ?? 20
      const form = formOf(input, grammar)
      const rebuilt = yield* rebuiltOn
      if (options.citedOn !== undefined && rebuilt !== null && options.citedOn < rebuilt) {
        return {
          input,
          form,
          status: "pre-rebuild",
          calls: [],
          total: 0,
          note: `cited ${options.citedOn}, before the store was rebuilt on ${rebuilt}: every id then restarted, ` +
            "so it names no call of this store — resolve the call by its Call-ID or capture"
        } satisfies Resolution
      }

      if (form === "run" || form === "run-case") {
        const match = form === "run" ? RUN.exec(input)! : RUN_CASE.exec(input)!
        const id = Number(match[1])
        const run = yield* sql<{
          capture: string
          lane: string
          env: string | null
          status: string
          started_at: Date
          prep_id: string | null
        }>`
          SELECT capture, lane, env, status, started_at, prep_id FROM runs WHERE id = ${id}
        `
        if (run.length === 0) {
          const kept = yield* summarisedRun(input, form, id, form === "run" ? undefined : match[2]!)
          if (kept !== undefined) return kept
          const { highest, never } = yield* neverIssued(id)
          return {
            input,
            form,
            status: never ? "pre-rebuild" : "not-found",
            calls: [],
            total: 0,
            note: never
              ? `run ${id} was never issued by this store (highest ${highest}, skipped ids aside): ` +
                "a run of before the rebuild" + (rebuilt === null ? "" : ` of ${rebuilt}`)
              : `run ${id} is not in the store (removed, and no citation kept its summary)`
          } satisfies Resolution
        }
        const { capture, lane, env, status } = run[0]!
        const pin = run[0]!.prep_id === null ? null : Number(run[0]!.prep_id)
        // Every run-based answer says which run it read, so a citation of another store's run shows.
        const said = `run ${id} (started ${run[0]!.started_at.toISOString()}, ${capture}, ` +
          `${pin === null ? "no prep" : `prep${pin}`})`
        if (form === "run") {
          return {
            input,
            form,
            status: "a-run",
            calls: [],
            total: 0,
            note: `${said} is a run, not a call: on ${lane}, env ${env ?? "unknown"}, ${status}`
          } satisfies Resolution
        }
        const named = match[2]!
        const caseId = shapes.suffix.test(named) ? `${capture}-${named}` : named
        const atPin = pin === null ? [] : yield* callAsOf(capture, caseId, pin)
        const found = atPin.length > 0
          ? answer(input, form, atPin, caseId)
          : yield* byCaseId(input, form, caseId)
        const held = yield* sql<{ case_id: string }>`
          SELECT case_id FROM case_results WHERE run_id = ${id} AND case_id = ${caseId}
        `
        return {
          ...found,
          note: held.length === 0 && found.status === "not-found"
            ? `${said} holds no case ${caseId}`
            : `${said}: ${found.note}`
        }
      }

      if (form === "bare-case") {
        const suffix = shapes.bare.exec(input)![1]!
        const rows = yield* callsWhere((column) => sql`${column} LIKE ${`%-${literal(suffix)}`}`, limit)
        return answer(input, form, rows, `-${suffix}`)
      }

      if (form === "pattern") {
        const pattern = input.split(ELISION).map(literal).join("%")
        const rows = yield* callsWhere((column) => sql`${column} LIKE ${pattern}`, limit)
        return answer(input, form, rows, `"${input}"`)
      }

      if (form === "case-id") return yield* byCaseId(input, form, input)

      if (form === "capture") {
        const captures = yield* sql<{ capture: string; cases: string; refusals: string; prep_id: string | null }>`
          WITH matched AS (
            SELECT DISTINCT ON (capture) capture, prep_id FROM (
              SELECT capture, prep_id FROM capture_cuts
              UNION ALL SELECT capture, prep_id FROM case_maps
              UNION ALL SELECT capture, NULL::bigint FROM case_ids
            ) readings
            WHERE capture = ${input} OR left(capture, ${input.length + 1}) = ${`${input}.`}
            ORDER BY capture, prep_id DESC NULLS LAST
          )
          SELECT m.capture, m.prep_id,
                 CASE WHEN m.prep_id IS NULL
                   THEN (SELECT count(*) FROM case_ids i WHERE i.capture = m.capture)
                   ELSE greatest(
                     (SELECT count(*) FROM cut_cases c WHERE c.prep_id = m.prep_id AND c.capture = m.capture),
                     (SELECT count(*) FROM case_maps c
                      WHERE c.prep_id = m.prep_id AND c.capture = m.capture AND c.kind = 'case')
                   )
                 END AS cases,
                 CASE WHEN m.prep_id IS NULL THEN 0 ELSE greatest(
                   (SELECT count(*) FROM cut_refusals r WHERE r.prep_id = m.prep_id AND r.capture = m.capture),
                   (SELECT count(*) FROM case_maps r
                    WHERE r.prep_id = m.prep_id AND r.capture = m.capture AND r.kind = 'refused')
                 ) END AS refusals
          FROM matched m ORDER BY m.capture
        `
        if (captures.length > 0) {
          return {
            input,
            form,
            status: "a-capture",
            calls: [],
            total: 0,
            note: captures.map((one) =>
              one.prep_id === null
                ? `${one.capture} is a capture, not a call: ${one.cases} frozen id(s), no reading held`
                : `${one.capture} is a capture, not a call: prep${one.prep_id} cut ${one.cases} case(s), ` +
                  `refused ${one.refusals}`
            ).join("; ")
          } satisfies Resolution
        }
      }

      // A Call-ID, or a call key: the cases keyed by it, then the refusals naming it.
      const byKey = (column: string) =>
        sql`(${sql.literal(column)} = ${input} OR left(${sql.literal(column)}, ${input.length + 1}) = ${`${input}#`})`
      const keyed = yield* newestOf(
        sql`
          SELECT capture, case_id, call_key, prep_id, 'case'::text AS kind, NULL::text[] AS reasons, 0 AS rank
          FROM cut_cases WHERE ${byKey("call_key")}
          UNION ALL
          SELECT r.capture, r.case_id, ${input}::text, r.prep_id, 'refused', r.reasons, 0
          FROM cut_refusal_calls k
          JOIN cut_refusals r ON r.prep_id = k.prep_id AND r.capture = k.capture AND r.case_id = k.case_id
          WHERE k.call_id = ${input}
          UNION ALL
          SELECT capture, case_id, coalesce(call_key, ${input}::text), prep_id, kind,
                 CASE WHEN kind = 'case' THEN NULL ELSE reasons END, 1
          FROM case_maps
          WHERE (kind = 'case' AND ${byKey("call_key")}) OR (kind = 'refused' AND call_ids @> ARRAY[${input}::text])
          UNION ALL
          SELECT capture, case_id, call_key, NULL::bigint, 'case', NULL::text[], 2
          FROM case_ids WHERE ${byKey("call_key")}
        `,
        limit
      )
      const resolved = answer(input, "call-id", keyed, `by Call-ID ${input}`)
      return resolved.status === "not-found"
        ? { ...resolved, note: `nothing the store holds is named "${input}": one of ${FORMS}` }
        : resolved
    })

    return Service.of({ resolve })
  })
)

/** The resolver reading capture names as packet-capture files ({@link DEFAULT_GRAMMAR}). */
export const layer = layerWith(DEFAULT_GRAMMAR)
