/**
 * The documents of the fixture source, and those of cases stating an
 * operation step at each point of each shape, pass the real `pivot-schema lint`:
 * `PIVOT_SCHEMA_BIN`, else the release build (`Binaries.releaseBinary`). The
 * binary missing, each test fails naming the path tried and how to build it;
 * `TOOLCHAIN_TEST_SKIP=1` skips them instead.
 */
import { NodeServices } from "@effect/platform-node"
import { Pivot } from "@sip/contracts"
import { CaseSource } from "@sip/rig"
import { Binaries, PivotSchemaCli } from "@sip/toolchain"
import * as ConfigProvider from "effect/ConfigProvider"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { defineBasicCase } from "../src/index.js"

const SOURCE = fileURLToPath(new URL("./fixtures/cases.ts", import.meta.url))

/** The binary the lint runs. */
const BIN: string = process.env["PIVOT_SCHEMA_BIN"] ?? Binaries.releaseBinary("pivot-schema")

const lintLayer = PivotSchemaCli.layer.pipe(
  Layer.provide(NodeServices.layer),
  Layer.provide(ConfigProvider.layer(ConfigProvider.fromUnknown({ PIVOT_SCHEMA_BIN: BIN })))
)

/** Every diagnostic the lint states on `document`, each `severity rule at path`. */
const diagnosticsOf = (scenario: string) =>
  Effect.runPromise(
    Effect.gen(function*() {
      const cli = yield* PivotSchemaCli.Service
      const report = yield* cli.lint(scenario)
      return report.diagnostics.map((d) => `${d.severity} ${d.rule} at ${d.path}: ${d.message}`)
    }).pipe(Effect.provide(lintLayer))
  )

let dir: string
beforeEach(() => {
  if (BIN.length === 0 || !fs.existsSync(BIN)) {
    throw new Error(
      `no pivot-schema at ${BIN}: build it with \`cargo build --release -p pivot-schema\`, or name one ` +
        "with PIVOT_SCHEMA_BIN, or skip the binary-bound suites with TOOLCHAIN_TEST_SKIP=1"
    )
  }
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "basic-case-lint-"))
})
afterEach(() => {
  if (dir !== undefined) fs.rmSync(dir, { recursive: true, force: true })
})

describe.skipIf(process.env["TOOLCHAIN_TEST_SKIP"] === "1")("the fixture source's documents", () => {
  it("pass the pivot schema lint with no error", async () => {
    const cells = await Effect.runPromise(CaseSource.load(SOURCE))
    expect(cells.map((cell) => cell.id)).toEqual(["alice-calls-bob", "bob-is-busy"])
    for (const cell of cells) {
      const scenario = path.join(dir, `${cell.id}.json`)
      fs.writeFileSync(scenario, Pivot.emitPivot(cell.document))
      const errors = (await diagnosticsOf(scenario)).filter((d) => d.startsWith("error "))
      expect([cell.id, errors]).toEqual([cell.id, []])
    }
  })
})

describe.skipIf(process.env["TOOLCHAIN_TEST_SKIP"] === "1")("a case stating an operation step", () => {
  it("passes the pivot schema lint with no error at each point of each shape", async () => {
    const parties = { caller: "sip:alice@atlanta.example.com", callee: "sip:bob@biloxi.example.com", final: [] }
    const cells = [
      defineBasicCase({ title: "answered", shape: "basic_call", ...parties, steps: [{ operation: "restart-worker", at: "answered" }] }),
      defineBasicCase({ title: "reached", shape: "basic_call", ...parties, steps: [{ operation: "restart-worker", at: "reached" }] }),
      defineBasicCase({ title: "rejected", shape: "invite_reject", ...parties, steps: [{ operation: "restart-worker", at: "reached" }] })
    ]
    for (const cell of cells) {
      expect(cell.document.flow.some((node) => node.op === "inject")).toBe(true)
      const scenario = path.join(dir, `${cell.id}.json`)
      fs.writeFileSync(scenario, Pivot.emitPivot(cell.document))
      const errors = (await diagnosticsOf(scenario)).filter((d) => d.startsWith("error "))
      expect([cell.id, errors]).toEqual([cell.id, []])
    }
  })
})
