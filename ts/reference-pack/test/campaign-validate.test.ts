/**
 * `siprig-campaign validate` (`@sip/siprig` `campaign-cli.ts`) on the
 * reference pack `SIPRIG_PACK` names, with no workspace, no store and no lane
 * stood: its cases validate with exit 0, and `--out --expand` writes each
 * compiled document, the cells' descriptions and each cell's expanded
 * expectation. One seeded defect per level exits 1 with its line: a case
 * whose tag no workspace declares (compile), a document whose step waits
 * `after` a step it does not hold (schema), two cells filed under one case id
 * (consistency). A pivot schema tool that does not run is refused once,
 * exit 2. Each seeded source is a module under a scratch directory
 * re-exporting the pack's cells changed, named by its absolute path.
 *
 * Its `pivot-schema` is found as `campaign-cli.test.ts` finds it;
 * `TOOLCHAIN_TEST_SKIP=1` skips the suite.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, describe, expect, it } from "vitest"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")
const CLI = path.join(CHECKOUT, "ts", "siprig", "src", "campaign-cli.ts")
const TSX = path.join(CHECKOUT, "ts", "siprig", "node_modules", ".bin", "tsx")
const CASES = path.join(PACK, "cases.ts")

const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"
const work = fs.mkdtempSync(path.join(os.tmpdir(), "campaign-validate-"))
afterAll(() => fs.rmSync(work, { recursive: true, force: true }))

const PIVOT_SCHEMA = process.env["PIVOT_SCHEMA_BIN"] ?? path.join(CHECKOUT, "target/release/pivot-schema")

/** `siprig-campaign validate <args>` on the reference pack, linting with `pivotSchema`: its exit code, its stdout and its stderr. */
const validateWith = (pivotSchema: string, ...args: ReadonlyArray<string>) => {
  const env: NodeJS.ProcessEnv = { ...process.env, SIPRIG_PACK: PACK, SIPRIG_WORKSPACE: "", PIVOT_SCHEMA_BIN: pivotSchema }
  const ran = spawnSync(TSX, [CLI, "validate", ...args], { env, encoding: "utf8" })
  return { code: ran.status, stdout: ran.stdout, stderr: ran.stderr.split("\n").filter((line) => !/Deprecation|trace-deprecation/.test(line)) }
}

/** `siprig-campaign validate <args>` on the reference pack: its exit code, its stdout and its stderr. */
const validate = (...args: ReadonlyArray<string>) => validateWith(PIVOT_SCHEMA, ...args)

/** A case source `name` under the scratch directory: the pack's cells, `body` mapping them (`cells`). */
const seeded = (name: string, body: string): string => {
  const file = path.join(work, `${name}.ts`)
  fs.writeFileSync(file, `import cases from ${JSON.stringify(CASES)}\nconst cells = cases\nexport default ${body}\n`)
  return file
}

describe.skipIf(skipping)("siprig-campaign validate", () => {
  it("validates the pack's cases with exit 0, writing the documents, their descriptions and expansions under --out", () => {
    const out = path.join(work, "out")
    const { code, stderr, stdout } = validate("--out", out, "--expand")
    expect(code, stderr.join("\n")).toBe(0)
    expect(stdout).toContain("2 cell(s) from 1 case source(s), 0 problem(s)")
    const described = JSON.parse(fs.readFileSync(path.join(out, "cells.json"), "utf8")) as ReadonlyArray<{ id: string }>
    expect(described.map((cell) => cell.id)).toEqual(["alice-calls-bob", "bob-is-busy"])
    for (const id of ["alice-calls-bob", "bob-is-busy"]) {
      expect(fs.existsSync(path.join(out, "cells", id, "scenario.json"))).toBe(true)
    }
    const expanded = JSON.parse(fs.readFileSync(path.join(out, "cells", "bob-is-busy", "expanded.json"), "utf8"))
    expect(expanded.legs.A.messages.map((m: { message: string }) => m.message)).toContain("486 Busy Here")
    expect(expanded.claims.postconditions.claims["call-records"].count).toBe(1)
  })

  it("names a case whose tag no workspace declares as a compile problem, exit 1", () => {
    const { code, stderr } = validate("--sources", "./step-cases.ts")
    expect(code).toBe(1)
    expect(stderr.filter((line) => line !== "")).toEqual([
      "compile step-cases.ts: alice-calls-bob-through-a-restart: names restarts, no declared tag (none is declared); " +
      "alice-calls-bob-after-the-restart: names restarts, no declared tag (none is declared)"
    ])
  })

  it("names a document whose step waits after a step it does not hold as a schema problem, exit 1", () => {
    const source = seeded(
      "after-unknown",
      `cells.map((cell) => ({ ...cell, document: { ...cell.document, flow: cell.document.flow.map((node, i) => i === 1 ? { ...node, after: ["s99"] } : node) } }))`
    )
    const { code, stderr } = validate("--sources", source)
    expect(code).toBe(1)
    expect(stderr[0]).toMatch(new RegExp(`^schema ${source.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}: fails the pivot schema lint: alice-calls-bob: ref/after-unknown`))
  })

  it("names two cells filed under one case id as a consistency problem, exit 1", () => {
    const source = seeded("twin", `[...cells, { ...cells[0], id: "alice-calls-bob-again" }]`)
    const { code, stderr } = validate("--sources", source)
    expect(code).toBe(1)
    expect(stderr.filter((line) => line !== "")).toEqual([
      `consistency ${source}: the cell alice-calls-bob-again is filed under the case id alice calls bob, as the cell alice-calls-bob of ${source} is: ` +
      "the store keys a cell's results by it"
    ])
  })

  it("refuses a pivot schema tool that does not run once, naming it, exit 2", () => {
    const broken = path.join(work, "pivot-schema-broken")
    fs.writeFileSync(broken, "#!/bin/sh\necho 'cannot load its library' >&2\nexit 3\n", { mode: 0o755 })
    const { code, stderr } = validateWith(broken)
    expect(code).toBe(2)
    expect(stderr.filter((line) => line !== "")).toEqual([
      `siprig-campaign validate: the pivot schema tool ${broken} does not run: ${broken} schema pivot exited 3: cannot load its library`
    ])
  })
})
