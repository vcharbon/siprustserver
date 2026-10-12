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
 *   <capture>-caseN|cutN|auto|legsA+B   a full case id, the capture by its name or its stem
 *   <capture>, <capture stem>     a capture ({@link Grammar}), which is no call: its cases are counted
 *   anything else                 a Call-ID, or a call key
 *
 * A case id is looked up over every cut that named it (`cut_cases`, then
 * `cut_refusals`): the newest prep naming it answers, a refusal there answers
 * `refused` with its reasons, and a call the capture's standing cut no longer
 * names says so. The store was rebuilt on the date its first prep was cut, and
 * every id then restarted: a run id above the highest the store holds, or an
 * id the caller says was cited before that date, answers `pre-rebuild`.
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

/** One call the input names, as the newest cut naming it reads it. */
export interface Call {
  readonly capture: string
  readonly caseId: string
  /** The capture's own name for the call; null where only a run, no cut, names it. */
  readonly callKey: string | null
  /** The newest prep naming the case; null where no cut does. */
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
export const FORMS = "runN/caseN, <capture>-caseN|cutN|auto|legsA+B, caseN, <capture>…-auto, a capture, a Call-ID"

const SUFFIX = /^(case\d+|cut\d+|auto|legs[\d+]+)$/
const CASE_ID = /^(.+)-(case\d+|cut\d+|auto|legs[\d+]+)$/
const BARE = /^-?(case\d+|cut\d+|auto)$/
const RUN = /^(?:@run:|run)?(\d+)$/
const RUN_CASE = /^(?:@run:|run)(\d+)\/(.+)$/
const ELISION = /…|\.\.\./

/**
 * The deployment's half of the grammar: which inputs name a CAPTURE. The case
 * suffixes (`-caseN`, `-cutN`, `-auto`, `-legsA+B`) are the cut's own and read
 * the same everywhere; how a capture is named is the deployment's corpus
 * convention.
 */
export interface Grammar {
  /** Whether an input that is no case id names a capture, by its file name or its stem. */
  readonly isCapture: (input: string) => boolean
}

/** A capture is a packet-capture file: its name carries `.pcap` (`.pcapng` included). */
export const DEFAULT_GRAMMAR: Grammar = { isCapture: (input) => input.includes(".pcap") }

/** How an input reads, before any lookup. */
export const formOf = (input: string, grammar: Grammar = DEFAULT_GRAMMAR): Form => {
  if (RUN_CASE.test(input)) return "run-case"
  if (RUN.test(input)) return "run"
  if (BARE.test(input)) return "bare-case"
  if (ELISION.test(input)) return "pattern"
  if (CASE_ID.test(input)) return "case-id"
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
  standing: row.standing
})

/** The resolver reading capture names by `grammar`. */
export const layerWith = (grammar: Grammar) => Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    /** When the store's ids began: its first prep's cut, as `YYYY-MM-DD`; null for an empty store. */
    const rebuiltOn = Effect.gen(function*() {
      const rows = yield* sql<{ at: Date | null }>`SELECT min(cut_at) AS at FROM capture_cuts`
      const at = rows[0]?.at ?? null
      return at === null ? null : at.toISOString().slice(0, 10)
    })

    /**
     * The calls whose case id `matches` admits, each at the newest prep naming
     * it — a cut case or a refusal — with the total before the limit.
     */
    const callsWhere = (matches: (column: Fragment) => Fragment, limit: number) =>
      sql<CallRow>`
        WITH named AS (
          SELECT capture, case_id, call_key, prep_id, 'case'::text AS kind, NULL::text[] AS reasons
          FROM cut_cases WHERE ${matches(sql.literal("case_id"))}
          UNION ALL
          SELECT capture, case_id,
                 (SELECT id FROM unnest(call_ids) AS id ORDER BY length(id), id LIMIT 1),
                 prep_id, 'refused', reasons
          FROM cut_refusals WHERE ${matches(sql.literal("case_id"))}
        ),
        newest AS (
          SELECT DISTINCT ON (capture, case_id) * FROM named ORDER BY capture, case_id, prep_id DESC
        )
        SELECT n.*,
               n.prep_id = (SELECT max(c.prep_id) FROM capture_cuts c WHERE c.capture = n.capture) AS standing,
               count(*) OVER () AS total
        FROM newest n
        ORDER BY n.capture, n.case_id
        LIMIT ${limit}
      `

    /**
     * The call a capture's case id named in the cut a run pinned: the
     * capture's newest cut no later than the pin, its case or its refusal.
     */
    const callAsOf = (capture: string, caseId: string, pin: number) =>
      sql<CallRow>`
        WITH cut AS (
          SELECT prep_id FROM capture_cuts WHERE capture = ${capture} AND prep_id <= ${pin}
          ORDER BY prep_id DESC LIMIT 1
        ),
        named AS (
          SELECT c.capture, c.case_id, c.call_key, c.prep_id, 'case'::text AS kind, NULL::text[] AS reasons
          FROM cut_cases c JOIN cut ON cut.prep_id = c.prep_id
          WHERE c.capture = ${capture} AND c.case_id = ${caseId}
          UNION ALL
          SELECT r.capture, r.case_id,
                 (SELECT id FROM unnest(r.call_ids) AS id ORDER BY length(id), id LIMIT 1),
                 r.prep_id, 'refused', r.reasons
          FROM cut_refusals r JOIN cut ON cut.prep_id = r.prep_id
          WHERE r.capture = ${capture} AND r.case_id = ${caseId}
        )
        SELECT n.*,
               n.prep_id = (SELECT max(c.prep_id) FROM capture_cuts c WHERE c.capture = n.capture) AS standing,
               count(*) OVER () AS total
        FROM named n
      `

    /** A case id no cut names but a run replayed: the run is the only witness. */
    const ranOnly = (caseId: string) =>
      sql<{ capture: string }>`
        SELECT r.capture FROM case_results cr JOIN runs r ON r.id = cr.run_id
        WHERE cr.case_id = ${caseId}
        ORDER BY r.id DESC LIMIT 1
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
        return { input, form, status: "not-found", calls, total, note: `no cut names ${what}` }
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
      const where = only.standing ? "" : ` (not in the capture's standing cut; last named by prep${only.prepId})`
      return only.kind === "refused"
        ? {
          input,
          form,
          status: "refused",
          calls,
          total,
          note: `refused by prep${only.prepId}: ${only.reasons.join(", ")}${where}`
        }
        : { input, form, status: "resolved", calls, total, note: `prep${only.prepId}${where}` }
    }

    const byCaseId = Effect.fn("Resolver.byCaseId")(function*(input: string, form: Form, caseId: string) {
      const rows = yield* callsWhere((column) => sql`${column} = ${caseId}`, 2)
      if (rows.length > 0) return answer(input, form, rows, caseId)
      // A capture cited by its stem: `trace_205845-case1` for `trace_205845.pcap.gz-case1`.
      const parts = CASE_ID.exec(caseId)
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
            callKey: null,
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
          const top = yield* sql<{ id: string | null }>`SELECT max(id) AS id FROM runs`
          const highest = Number(top[0]?.id ?? 0)
          return {
            input,
            form,
            status: id > highest ? "pre-rebuild" : "not-found",
            calls: [],
            total: 0,
            note: id > highest
              ? `run ${id} is above the store's highest run ${highest}: a run of before the rebuild` +
                (rebuilt === null ? "" : ` of ${rebuilt}`)
              : `run ${id} is not in the store (removed)`
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
        const caseId = SUFFIX.test(named) ? `${capture}-${named}` : named
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
        const suffix = BARE.exec(input)![1]!
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
        const captures = yield* sql<{ capture: string; cases: string; refusals: string; prep_id: string }>`
          WITH matched AS (
            SELECT DISTINCT ON (capture) capture, prep_id FROM capture_cuts
            WHERE capture = ${input} OR left(capture, ${input.length + 1}) = ${`${input}.`}
            ORDER BY capture, prep_id DESC
          )
          SELECT m.capture, m.prep_id,
                 (SELECT count(*) FROM cut_cases c WHERE c.prep_id = m.prep_id AND c.capture = m.capture) AS cases,
                 (SELECT count(*) FROM cut_refusals r WHERE r.prep_id = m.prep_id AND r.capture = m.capture) AS refusals
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
              `${one.capture} is a capture, not a call: prep${one.prep_id} cut ${one.cases} case(s), ` +
              `refused ${one.refusals}`
            ).join("; ")
          } satisfies Resolution
        }
      }

      // A Call-ID, or a call key: the cases keyed by it, then the refusals naming it.
      const keyed = yield* sql<CallRow>`
        WITH named AS (
          SELECT capture, case_id, call_key, prep_id, 'case'::text AS kind, NULL::text[] AS reasons
          FROM cut_cases
          WHERE call_key = ${input} OR left(call_key, ${input.length + 1}) = ${`${input}#`}
          UNION ALL
          SELECT r.capture, r.case_id, ${input}::text, r.prep_id, 'refused', r.reasons
          FROM cut_refusal_calls k
          JOIN cut_refusals r ON r.prep_id = k.prep_id AND r.capture = k.capture AND r.case_id = k.case_id
          WHERE k.call_id = ${input}
        ),
        newest AS (
          SELECT DISTINCT ON (capture, case_id) * FROM named ORDER BY capture, case_id, prep_id DESC
        )
        SELECT n.*,
               n.prep_id = (SELECT max(c.prep_id) FROM capture_cuts c WHERE c.capture = n.capture) AS standing,
               count(*) OVER () AS total
        FROM newest n
        ORDER BY n.capture, n.case_id
        LIMIT ${limit}
      `
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
