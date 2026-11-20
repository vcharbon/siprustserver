/**
 * What a cell is answered AFTER its result landed (ADR-0060 appendix §5, §8,
 * §9): a collector's verification lines merged into the lines the cell holds,
 * and its attachments added to the cell's, in one transaction. A merged line
 * replaces the cell's `pending` line of its name and source where it stands;
 * any other follows the cell's lines, so a line already answered is never
 * overwritten. The cell's final status is read off the merged lines
 * (`cell_statuses`), as ever.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as Attachments from "./attachments.js"
import * as Sessions from "./sessions.js"
import * as Verifications from "./verifications.js"

/** A cell's answers from one source: its lines, and its attachments listed in its directory. */
export interface Answers {
  readonly verifications: ReadonlyArray<Verifications.Line>
  readonly held: Attachments.Held
}

/**
 * `held` with `answered` merged in: each answered line in place of the held
 * `pending` one of its name and source, else after them; a held line that
 * answered stands.
 */
export const merged = (
  held: ReadonlyArray<Verifications.Line>,
  answered: ReadonlyArray<Verifications.Line>
): ReadonlyArray<Verifications.Line> => {
  const out = [...held]
  for (const line of answered) {
    const at = out.findIndex((one) => one.name === line.name && one.source === line.source && one.answer === "pending")
    if (at >= 0) out[at] = line
    else out.push(line)
  }
  return out
}

export interface Interface {
  /** `answers` merged into the landed cell `caseId` of run `runId`. */
  readonly merge: (runId: number, caseId: string, answers: Answers) => Effect.Effect<void, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/CellAnswers") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    const merge = (runId: number, caseId: string, answers: Answers) =>
      sql.withTransaction(Effect.gen(function*() {
        const holder = { scope: "cell", runId, caseId } as const
        const rows = yield* sql<Verifications.Row>`
          SELECT * FROM verifications
          WHERE scope = 'cell' AND run_id = ${runId} AND case_id = ${caseId}
          ORDER BY ordinal
          FOR UPDATE
        `
        if (answers.verifications.length > 0) {
          yield* Verifications.replace(holder, merged(rows.map(Verifications.lineOf), answers.verifications))
        }
        if (answers.held.attachments.length > 0) yield* Attachments.addTo(holder, answers.held)
      })).pipe(Effect.provideService(SqlClient, sql), Effect.provideService(Sessions.Service, root))

    return Service.of({ merge })
  })
)
