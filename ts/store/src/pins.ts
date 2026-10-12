/**
 * PINS: the runs kept by hand, each with the record that asked for it. A
 * pinned run and its bundle survive every retention pass and the wipe; a run
 * removed by an explicit delete takes its pin with it.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

export interface Pin {
  readonly runId: number
  /** The record that asked for it; null where none was named. */
  readonly ticket: string | null
  readonly pinnedAt: string
}

export interface Interface {
  /** Pins runs, a run already pinned keeping its first record; answers the runs pinned now. */
  readonly pin: (runIds: ReadonlyArray<number>, ticket: string | null) => Effect.Effect<ReadonlyArray<number>, SqlError>
  /** Unpins runs; answers the runs that were pinned. */
  readonly unpin: (runIds: ReadonlyArray<number>) => Effect.Effect<ReadonlyArray<number>, SqlError>
  /** Every pin, newest first. */
  readonly all: Effect.Effect<ReadonlyArray<Pin>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Pins") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const pin = Effect.fn("Pins.pin")(function*(runIds: ReadonlyArray<number>, ticket: string | null) {
      if (runIds.length === 0) return []
      const rows = yield* sql<{ run_id: string }>`
        INSERT INTO pins (run_id, ticket)
        SELECT id, ${ticket} FROM runs WHERE id IN ${sql.in(runIds)}
        ON CONFLICT (run_id) DO NOTHING
        RETURNING run_id
      `
      return rows.map((row) => Number(row.run_id)).sort((a, b) => a - b)
    })

    const unpin = Effect.fn("Pins.unpin")(function*(runIds: ReadonlyArray<number>) {
      if (runIds.length === 0) return []
      const rows = yield* sql<{ run_id: string }>`
        DELETE FROM pins WHERE run_id IN ${sql.in(runIds)} RETURNING run_id
      `
      return rows.map((row) => Number(row.run_id)).sort((a, b) => a - b)
    })

    const all = Effect.gen(function*() {
      const rows = yield* sql<{ run_id: string; ticket: string | null; pinned_at: Date }>`
        SELECT run_id, ticket, pinned_at FROM pins ORDER BY pinned_at DESC, run_id DESC
      `
      return rows.map((row): Pin => ({
        runId: Number(row.run_id),
        ticket: row.ticket,
        pinnedAt: row.pinned_at.toISOString()
      }))
    })

    return Service.of({ pin, unpin, all })
  })
)
