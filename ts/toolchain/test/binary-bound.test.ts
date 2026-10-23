/**
 * The suites held to the REAL `pivot-schema` (`schema-conformance`,
 * `pivot-schema-schedules`) fail where the binary is not on disk, naming the
 * path they tried and how to build it, unless the run states
 * `TOOLCHAIN_TEST_SKIP=1`, which skips them instead.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"

const PACKAGE = path.resolve(import.meta.dirname, "..")
const SUITES = ["test/schema-conformance.test.ts", "test/pivot-schema-schedules.test.ts"]

/** A path no binary is at. */
const MISSING = "/nonexistent-binary-bound-check/pivot-schema"

interface FileResult {
  readonly name: string
  readonly assertionResults: ReadonlyArray<{
    readonly ancestorTitles: ReadonlyArray<string>
    readonly status: string
    readonly failureMessages: ReadonlyArray<string>
  }>
}

/** This package's vitest over the binary-bound suites, its JSON report read back. */
const vitest = (extra: NodeJS.ProcessEnv) => {
  const env: NodeJS.ProcessEnv = { ...process.env, PIVOT_SCHEMA_BIN: MISSING, ...extra }
  if (extra["TOOLCHAIN_TEST_SKIP"] === undefined) delete env["TOOLCHAIN_TEST_SKIP"]
  const out = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "binary-bound-")), "report.json")
  const run = spawnSync(
    path.join(PACKAGE, "node_modules", ".bin", "vitest"),
    ["run", "--reporter=json", `--outputFile=${out}`, ...SUITES],
    { cwd: PACKAGE, env, encoding: "utf8", timeout: 120_000 }
  )
  const report = fs.existsSync(out)
    ? JSON.parse(fs.readFileSync(out, "utf8")) as { testResults: ReadonlyArray<FileResult> }
    : { testResults: [] }
  fs.rmSync(path.dirname(out), { recursive: true, force: true })
  return { status: run.status, output: `${run.stdout}${run.stderr}`, results: report.testResults }
}

/** The tests of the binary-bound blocks: every test outside the stub-driven adapter block. */
const bound = (results: ReadonlyArray<FileResult>) =>
  results.flatMap((file) => file.assertionResults).filter((test) => test.ancestorTitles[0] !== "the schedules adapter")

describe("the suites held to the real pivot-schema", () => {
  it("fail where the binary is missing, naming it and its build", () => {
    const run = vitest({})
    expect(run.status, run.output).not.toBe(0)
    const tests = bound(run.results)
    expect(tests.length).toBeGreaterThan(0)
    for (const test of tests) {
      expect(test.status).toBe("failed")
      const message = test.failureMessages.join("\n")
      expect(message).toContain(`no pivot-schema at ${MISSING}`)
      expect(message).toContain("cargo build --release -p pivot-schema")
      expect(message).toContain("TOOLCHAIN_TEST_SKIP=1")
    }
  }, 150_000)

  it("skip where the binary is missing and the run opts out", () => {
    const run = vitest({ TOOLCHAIN_TEST_SKIP: "1" })
    expect(run.status, run.output).toBe(0)
    const tests = bound(run.results)
    expect(tests.length).toBeGreaterThan(0)
    expect(tests.every((test) => test.status === "skipped" || test.status === "pending")).toBe(true)
  }, 150_000)
})
