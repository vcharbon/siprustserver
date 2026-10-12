/**
 * The REPLAY REQUEST: a list of request cells — (capture, call key)
 * — with the lane, the knobs and the selection it was built from. A request
 * names calls, never case ids, so each replay of it RESOLVES the cells against
 * the prep it pins: the case a key names in that cut, or why it names none.
 *
 * Freezing a request is one insert of N rows; resolving it is one read of the
 * cuts over every capture it touches, one refusal lookup per capture with a
 * key the cut holds no case for, and one insert of the cells a whole-capture
 * selection grew by. What a job then ran is `job_cells`, one row per cell.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as Cuts from "./cuts.js"
import type { CutCase, Gone } from "./model.js"
import { isGone } from "./model.js"

/** One cell: the capture and the key of one call of it. */
export interface Cell {
  readonly capture: string
  readonly callKey: string
}

/** Who put a cell on the request: the selection, or a resolution keeping a whole capture whole. */
export type Origin = "selected" | "added_by_prep"

export interface RequestCell extends Cell {
  readonly origin: Origin
}

/**
 * What a request was built from, kept so the request reads as a sentence.
 * `whole` names the captures the selection meant WHOLE — every call of them,
 * today's and a re-cut's — which is what grows at resolution: a
 * capture list, a universe (minus the captures a span bound kept in part), and
 * the filter bar at scope `all` with no chip. A filter that names calls by
 * what they did, and a ticked list, mean the frozen keys and nothing more.
 */
export type Selection =
  | {
    readonly kind: "filter"
    /** The lane whose queue the words were read on; absent, the request's own. */
    readonly lane?: string
    readonly scope: string
    readonly q?: string
    readonly rule?: string
    readonly expected?: string
    readonly refusal?: string
    /** The bar's outcome chip. */
    readonly outcome?: string
    /** The values the bar hid per chip group, as `not-<group>` carries them. */
    readonly exclude?: Readonly<Partial<Record<string, ReadonlyArray<string>>>>
    readonly whole: ReadonlyArray<string>
  }
  | { readonly kind: "captures"; readonly captures: ReadonlyArray<string>; readonly whole: ReadonlyArray<string> }
  | {
    readonly kind: "universe"
    readonly universe: "all" | "standing"
    readonly maxSpan?: number
    readonly greenOn?: string
    readonly sample?: number
    readonly limit?: number
    readonly whole: ReadonlyArray<string>
  }
  | { readonly kind: "cells"; readonly file: string; readonly whole: ReadonlyArray<string> }

/** The replay knobs a request carries; the lane clamps them when it runs. */
export interface Knobs {
  readonly concurrency: number
  readonly batchSize: number
  readonly memoryMax: string
  readonly startsPerSecond: number
  readonly cleanup: boolean
  /** How many passes a job of the request replays its cells in; one where absent. */
  readonly repeat?: number
}

export interface CreateRequest {
  readonly lane: string
  readonly selection: Selection
  readonly knobs: Knobs
  readonly createdBy: string
  readonly cells: ReadonlyArray<Cell>
}

export interface Created {
  readonly id: number
  readonly cells: number
  readonly captures: ReadonlyArray<string>
}

/** One request as a reader sees it. */
export interface Row {
  readonly id: number
  readonly lane: string
  readonly selection: Selection
  readonly knobs: Knobs
  readonly createdAt: string
  readonly createdBy: string
  readonly cells: number
  readonly captures: number
  readonly jobs: number
}

/** How a job's cells ended, counted. */
export interface JobSummary {
  readonly jobId: number
  readonly planned: number
  readonly ran: number
  readonly gone: number
}

export interface Detail {
  readonly request: Row
  readonly cells: ReadonlyArray<RequestCell>
  readonly jobs: ReadonlyArray<JobSummary>
}

/** A cell the pinned cut holds a case for. */
export interface PlannedCell extends RequestCell, CutCase {}

/** A cell the pinned cut holds no case for, and why. */
export interface GoneCell extends RequestCell, Gone {}

/**
 * One capture of a request, resolved: `full` when the request holds every
 * key of the capture's pinned cut (the set kind, derived here and never
 * asked), `cases` otherwise — a capture the pin holds no cut of included.
 * `added` is what this resolution put on the request to keep it whole.
 */
export interface CaptureResolution {
  readonly capture: string
  readonly kind: "full" | "cases"
  readonly planned: ReadonlyArray<PlannedCell>
  readonly gone: ReadonlyArray<GoneCell>
  readonly added: ReadonlyArray<string>
}

export interface Resolution {
  readonly requestId: number
  readonly prepId: number
  readonly lane: string
  readonly knobs: Knobs
  /** In capture order. */
  readonly captures: ReadonlyArray<CaptureResolution>
}

/** How a job cell stands: what the resolution made of it, then whether the job ran it. */
export type Outcome = "planned" | "gone: refused" | "gone: not in this cut" | "ran"

export interface JobCell extends Cell {
  readonly caseId: string | null
  readonly caseHash: string | null
  readonly outcome: Outcome
  readonly reasons: ReadonlyArray<string>
}

export interface Interface {
  /** Freezes the cells as one request: one insert, whatever N is. */
  readonly create: (request: CreateRequest) => Effect.Effect<Created, SqlError>
  readonly list: (limit: number) => Effect.Effect<ReadonlyArray<Row>, SqlError>
  /** The request's row alone — its sizes, no cell; null where no request has the id. */
  readonly get: (id: number) => Effect.Effect<Row | null, SqlError>
  /** The request, its cells, its jobs; null where no request has the id. */
  readonly show: (id: number) => Effect.Effect<Detail | null, SqlError>
  /**
   * The request's cells against the cut standing at `prepId`: per
   * capture, the cells the cut holds a case for, the gone ones with their
   * reasons, and — for a capture the selection meant whole — the keys the cut
   * holds that the request had none of, ADDED as `added_by_prep` cells. Null
   * where no request has the id.
   */
  readonly resolve: (id: number, prepId: number) => Effect.Effect<Resolution | null, SqlError>
  /** Records what a job resolved every cell to, one insert. */
  readonly recordJob: (jobId: number, resolution: Resolution) => Effect.Effect<number, SqlError>
  /** Marks the cells a job ran. */
  readonly ran: (jobId: number, cells: ReadonlyArray<Cell>) => Effect.Effect<void, SqlError>
  /**
   * The request's cells no job of it has run: what a resume replays.
   * A cell never planned under any job counts too, and a cell a job marked
   * `gone` but no job ran is still unrun — the next resolution may have it.
   */
  readonly unran: (requestId: number) => Effect.Effect<ReadonlyArray<Cell>, SqlError>
  /** A job's cells, in cell order. */
  readonly jobCells: (jobId: number) => Effect.Effect<ReadonlyArray<JobCell>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Requests") {}

interface RequestRow {
  readonly id: string
  readonly lane: string
  readonly selection: Selection
  readonly knobs: Knobs
  readonly created_at: Date
  readonly created_by: string
  readonly cells: number
  readonly captures: number
  readonly jobs: number
}

const rowOf = (row: RequestRow): Row => ({
  id: Number(row.id),
  lane: row.lane,
  selection: row.selection,
  knobs: row.knobs,
  createdAt: row.created_at.toISOString(),
  createdBy: row.created_by,
  cells: Number(row.cells),
  captures: Number(row.captures),
  jobs: Number(row.jobs)
})

const text = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0)

const byCell = (a: Cell, b: Cell): number => text(a.capture, b.capture) || text(a.callKey, b.callKey)

/** The outcome a resolution gives a gone cell. */
const outcomeOf = (gone: Gone): Outcome => (gone.gone === "refused" ? "gone: refused" : "gone: not in this cut")

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const cuts = yield* Cuts.Service

    /** Every request with its sizes. */
    const summary = sql`
      SELECT r.id, r.lane, r.selection, r.knobs, r.created_at, r.created_by,
             (SELECT count(*)::int FROM request_cells c WHERE c.request_id = r.id) AS cells,
             (SELECT count(DISTINCT c.capture)::int FROM request_cells c WHERE c.request_id = r.id) AS captures,
             (SELECT count(DISTINCT j.job_id)::int FROM job_cells j WHERE j.request_id = r.id) AS jobs
      FROM requests r
    `

    /** One statement for N cells: the cells travel as one JSON list and unnest server-side. */
    const insertCells = (requestId: number, cells: ReadonlyArray<Cell>, origin: Origin) =>
      cells.length === 0
        ? Effect.void
        : sql`
          INSERT INTO request_cells (request_id, capture, call_key, origin)
          SELECT ${requestId}, c.capture, c."callKey", ${origin}
          FROM jsonb_to_recordset(${JSON.stringify(cells)}::jsonb) AS c(capture text, "callKey" text)
          ON CONFLICT DO NOTHING
        `

    const create = Effect.fn("Requests.create")(function*(request: CreateRequest) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        const rows = yield* sql<{ id: string }>`
          INSERT INTO requests (lane, selection, knobs, created_by)
          VALUES (${request.lane}, ${JSON.stringify(request.selection)}::jsonb,
                  ${JSON.stringify(request.knobs)}::jsonb, ${request.createdBy})
          RETURNING id
        `
        const id = Number(rows[0]!.id)
        yield* insertCells(id, request.cells, "selected")
        const counted = yield* sql<{ cells: number }>`
          SELECT count(*)::int AS cells FROM request_cells WHERE request_id = ${id}
        `
        return {
          id,
          cells: Number(counted[0]!.cells),
          captures: [...new Set(request.cells.map((cell) => cell.capture))].sort()
        } satisfies Created
      }))
    })

    const list = Effect.fn("Requests.list")(function*(limit: number) {
      const rows = yield* sql<RequestRow>`${summary} ORDER BY r.id DESC LIMIT ${limit}`
      return rows.map(rowOf)
    })

    const cellsOf = Effect.fn("Requests.cellsOf")(function*(id: number) {
      const rows = yield* sql<{ capture: string; call_key: string; origin: Origin }>`
        SELECT capture, call_key, origin FROM request_cells
        WHERE request_id = ${id}
        ORDER BY capture, call_key
      `
      return rows.map((row): RequestCell => ({ capture: row.capture, callKey: row.call_key, origin: row.origin }))
    })

    const get = Effect.fn("Requests.get")(function*(id: number) {
      const head = yield* sql<RequestRow>`${summary} WHERE r.id = ${id}`
      return head[0] === undefined ? null : rowOf(head[0])
    })

    const show = Effect.fn("Requests.show")(function*(id: number) {
      const head = yield* sql<RequestRow>`${summary} WHERE r.id = ${id}`
      const row = head[0]
      if (row === undefined) return null
      const jobs = yield* sql<{ job_id: string; planned: number; ran: number; gone: number }>`
        SELECT job_id,
               count(*) FILTER (WHERE outcome = 'planned')::int AS planned,
               count(*) FILTER (WHERE outcome = 'ran')::int AS ran,
               count(*) FILTER (WHERE outcome LIKE 'gone:%')::int AS gone
        FROM job_cells WHERE request_id = ${id}
        GROUP BY job_id ORDER BY job_id
      `
      return {
        request: rowOf(row),
        cells: yield* cellsOf(id),
        jobs: jobs.map((job): JobSummary => ({
          jobId: Number(job.job_id),
          planned: Number(job.planned),
          ran: Number(job.ran),
          gone: Number(job.gone)
        }))
      } satisfies Detail
    })

    const resolve = Effect.fn("Requests.resolve")(function*(id: number, prepId: number) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        const head = yield* sql<RequestRow>`${summary} WHERE r.id = ${id}`
        const row = head[0]
        if (row === undefined) return null
        const request = rowOf(row)
        const held = new Map<string, Array<RequestCell>>()
        for (const cell of yield* cellsOf(id)) {
          const list = held.get(cell.capture) ?? []
          list.push(cell)
          held.set(cell.capture, list)
        }
        // A whole capture with no cell yet — refused whole when frozen — is
        // still the request's, and a re-cut that made cases of it grows it.
        const whole = new Set(request.selection.whole)
        const captures = [...new Set([...held.keys(), ...whole])].sort()
        const cut = yield* cuts.cases(prepId, captures)
        const added: Array<Cell> = []
        const resolved: Array<CaptureResolution> = []
        for (const capture of captures) {
          const cells = held.get(capture) ?? []
          const cases = new Map((cut.get(capture) ?? []).map((one) => [one.callKey, one]))
          const keys = new Set(cells.map((cell) => cell.callKey))
          const grown: Array<string> = []
          if (whole.has(capture)) {
            for (const key of cases.keys()) {
              if (keys.has(key)) continue
              grown.push(key)
              keys.add(key)
              cells.push({ capture, callKey: key, origin: "added_by_prep" })
              added.push({ capture, callKey: key })
            }
          }
          const missing = cells.filter((cell) => !cases.has(cell.callKey))
          const gone = missing.length === 0
            ? new Map<string, Gone>()
            : new Map(
              [...yield* cuts.resolve(prepId, capture, missing.map((cell) => cell.callKey))]
                .flatMap(([key, one]) => (isGone(one) ? [[key, one] as const] : []))
            )
          const ordered = [...cells].sort(byCell)
          resolved.push({
            capture,
            kind: cases.size > 0 && [...cases.keys()].every((key) => keys.has(key)) ? "full" : "cases",
            planned: ordered.flatMap((cell) => {
              const one = cases.get(cell.callKey)
              return one === undefined ? [] : [{ ...cell, ...one } satisfies PlannedCell]
            }),
            gone: ordered.flatMap((cell) => {
              const why = gone.get(cell.callKey)
              return why === undefined ? [] : [{ ...cell, ...why } satisfies GoneCell]
            }),
            added: grown.sort()
          })
        }
        yield* insertCells(id, added, "added_by_prep")
        return {
          requestId: id,
          prepId,
          lane: request.lane,
          knobs: request.knobs,
          captures: resolved
        } satisfies Resolution
      }))
    })

    const recordJob = Effect.fn("Requests.recordJob")(function*(jobId: number, resolution: Resolution) {
      const cells = resolution.captures.flatMap((capture) => [
        ...capture.planned.map((cell) => ({
          capture: cell.capture,
          callKey: cell.callKey,
          caseId: cell.caseId,
          caseHash: cell.caseHash,
          outcome: "planned" satisfies Outcome,
          reasons: [] as ReadonlyArray<string>
        })),
        ...capture.gone.map((cell) => ({
          capture: cell.capture,
          callKey: cell.callKey,
          caseId: null,
          caseHash: null,
          outcome: outcomeOf(cell),
          reasons: cell.reasons
        }))
      ])
      if (cells.length === 0) return 0
      yield* sql`
        INSERT INTO job_cells (job_id, request_id, capture, call_key, case_id, case_hash, outcome, reasons)
        SELECT ${jobId}, ${resolution.requestId}, c.capture, c."callKey", c."caseId", c."caseHash",
               c.outcome, c.reasons
        FROM jsonb_to_recordset(${JSON.stringify(cells)}::jsonb)
          AS c(capture text, "callKey" text, "caseId" text, "caseHash" text, outcome text, reasons text[])
      `
      return cells.length
    })

    const ran = Effect.fn("Requests.ran")(function*(jobId: number, cells: ReadonlyArray<Cell>) {
      if (cells.length === 0) return
      yield* sql`
        UPDATE job_cells j SET outcome = 'ran'
        FROM jsonb_to_recordset(${JSON.stringify(cells)}::jsonb) AS c(capture text, "callKey" text)
        WHERE j.job_id = ${jobId} AND j.capture = c.capture AND j.call_key = c."callKey"
      `
    })

    const unran = Effect.fn("Requests.unran")(function*(requestId: number) {
      const rows = yield* sql<{ capture: string; call_key: string }>`
        SELECT c.capture, c.call_key
        FROM request_cells c
        WHERE c.request_id = ${requestId}
          AND NOT EXISTS (
            SELECT 1 FROM job_cells j
            WHERE j.request_id = c.request_id AND j.capture = c.capture AND j.call_key = c.call_key
              AND j.outcome = 'ran'
          )
        ORDER BY c.capture, c.call_key
      `
      return rows.map((row): Cell => ({ capture: row.capture, callKey: row.call_key }))
    })

    const jobCells = Effect.fn("Requests.jobCells")(function*(jobId: number) {
      const rows = yield* sql<{
        capture: string
        call_key: string
        case_id: string | null
        case_hash: string | null
        outcome: Outcome
        reasons: ReadonlyArray<string>
      }>`
        SELECT capture, call_key, case_id, case_hash, outcome, reasons
        FROM job_cells WHERE job_id = ${jobId}
        ORDER BY capture, call_key
      `
      return rows.map((row): JobCell => ({
        capture: row.capture,
        callKey: row.call_key,
        caseId: row.case_id,
        caseHash: row.case_hash,
        outcome: row.outcome,
        reasons: row.reasons
      }))
    })

    return Service.of({ create, list, get, show, resolve, recordJob, ran, unran, jobCells })
  })
)
