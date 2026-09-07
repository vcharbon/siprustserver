/**
 * `pivot-schema schedules` — the retransmission schedule table the Rust side
 * walks (`sip_retransmit::Schedule::rfc`, one row per class to its give-up),
 * decoded through `@sip/contracts/schedules`.
 *
 * Its own service beside {@link PivotSchemaCli}: the pipeline reads the
 * checked-in mirror and never spawns, so the one thing this adapter exists for
 * is the conformance test that holds that mirror to the binary's output.
 */
import { decodeScheduleTable, type ScheduleTable } from "@sip/contracts/schedules"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import type * as Schema from "effect/Schema"
import { PIVOT_SCHEMA } from "./binaries.js"
import { type CliFailed, type CliUnavailable, runner } from "./spawn.js"

export interface Interface {
  /** `pivot-schema schedules` → the decoded table, one row per class. */
  readonly schedules: () => Effect.Effect<ScheduleTable, CliFailed | CliUnavailable | Schema.SchemaError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/toolchain/PivotSchemaSchedules") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function* () {
    const cli = yield* runner
    const bin = yield* PIVOT_SCHEMA

    const schedules = Effect.fn("PivotSchemaSchedules.schedules")(function* () {
      const text = yield* cli.ok(bin, ["schedules"])
      return yield* decodeScheduleTable(JSON.parse(text) as unknown)
    })

    return Service.of({ schedules })
  })
)

export * as PivotSchemaSchedules from "./pivot-schema-schedules.js"
