/**
 * The schedule-table mirror against the REAL `pivot-schema schedules` — the
 * one place `@sip/contracts`' checked-in `SCHEDULES` is held to the ladder
 * `sip-retransmit` walks, byte for byte, the way the other Rust-owned formats
 * are held to `pivot-schema schema`.
 *
 * The adapter half runs against a stub, as the other adapters' tests do. The
 * conformance half needs the binary: `PIVOT_SCHEMA_BIN` names it, and absent
 * that the release build the toolchain defaults to (`releaseBinary`); with
 * neither on disk it is skipped, as `schema-conformance` is — a suite that
 * needs a `cargo build` is a suite nobody runs on a fresh checkout.
 */
import { Canonical } from "@sip/contracts"
import { SCHEDULES } from "@sip/contracts/schedules"
import { Binaries, PivotSchemaSchedules } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import { describe, expect, it } from "vitest"
import { fixture, rig } from "./harness.js"

const table = (env: Record<string, string>) =>
  Effect.runPromise(
    Effect.gen(function* () {
      const cli = yield* PivotSchemaSchedules.Service
      return yield* cli.schedules()
    }).pipe(Effect.provide(rig(PivotSchemaSchedules.layer, env)))
  )

describe("the schedules adapter", () => {
  it("decodes what the binary prints", async () => {
    const decoded = await table({ PIVOT_SCHEMA_BIN: fixture("stub-pivot-schema-schedules.sh") })
    expect(decoded).toEqual({
      classes: [{ class: "invite-client", give_up_ms: 32000, rung_intervals_ms: [500, 1000] }]
    })
  })

  it("fails when the binary refuses", async () => {
    const exit = await Effect.runPromiseExit(
      Effect.gen(function* () {
        const cli = yield* PivotSchemaSchedules.Service
        return yield* cli.schedules()
      }).pipe(Effect.provide(rig(PivotSchemaSchedules.layer, { PIVOT_SCHEMA_BIN: fixture("stub-pivot-schema.sh") })))
    )
    expect(exit._tag).toBe("Failure")
    expect(JSON.stringify(exit)).toContain("Toolchain.CliFailed")
  })
})

const BIN = process.env.PIVOT_SCHEMA_BIN ?? Binaries.releaseBinary("pivot-schema")
const available = BIN.length > 0 && fs.existsSync(BIN)

describe.skipIf(!available)("the checked-in schedule table", () => {
  it("is what the binary prints, byte for byte", async () => {
    const decoded = await table({ PIVOT_SCHEMA_BIN: BIN })
    expect(decoded).toEqual(SCHEDULES)
    // Decoding proves the shape; the raw bytes prove the mirror is the table
    // the binary prints and not a paraphrase of it — the same canonical
    // form on both sides, so a drift in either direction is a one-file diff.
    expect(execFileSync(BIN, ["schedules"], { encoding: "utf8" })).toBe(Canonical.format(SCHEDULES))
  })
})
