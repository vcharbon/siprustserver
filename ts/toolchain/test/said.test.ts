/**
 * What a dead command's row SAYS: the stack frames go, what the command said
 * stays — including the `at [...]` a decoder writes inside a message to name
 * the field it refused. The log it was written to keeps everything.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { Logged } from "../src/logged.js"
import { said } from "../src/said.js"

/** The real shape: node's header, the effect frames, then the error's own fields. */
const DEAD = [
  "/somewhere/checkout/ts/rules/src/data.ts:70",
  "  new DataLoadError({ file, reason: String(issue) })",
  "",
  "DataLoadError [Rules.DataLoadError]",
  "    at <anonymous> (/somewhere/checkout/ts/rules/src/data.ts:70:3)",
  "    at ~effect/Utils/internal (/somewhere/ts/node_modules/effect/src/Utils.ts:208:14)",
  "    at FiberImpl.runLoop (/somewhere/ts/node_modules/effect/src/internal/effect.ts:655:39)",
  "    at Module.<anonymous> (/somewhere/ts/node_modules/effect/src/internal/effect.ts:5527:18) {",
  "  _tag: 'Rules.DataLoadError',",
  "  file: '/somewhere/census/2026-08-28.census.json',",
  "  reason: 'SchemaError(Missing key",
  "  at [\"rules\"][\"second-answer-repeats-the-first\"])'",
  "}"
].join("\n")

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "said-test-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

describe("said", () => {
  it("drops the stack frames and keeps what the command said", () => {
    const tail = said(DEAD)
    expect(tail).toContain("Rules.DataLoadError")
    expect(tail).toContain("2026-08-28.census.json")
    expect(tail).not.toContain("FiberImpl.runLoop")
    expect(tail).not.toContain("~effect/Utils/internal")
    // Node opens the field dump on its last frame; the brace goes with it.
    expect(tail).not.toContain("Module.<anonymous>")
  })

  it("keeps the field a decoder names, which reads like a frame and is not one", () => {
    expect(said(DEAD)).toContain('at ["rules"]["second-answer-repeats-the-first"]')
  })
})

describe("Logged.section", () => {
  it("appends the run to the log whole, frames included, and answers its last words without them", async () => {
    const file = path.join(dir, "generator.log")
    fs.writeFileSync(file, "earlier\n")
    const stderr = path.join(dir, "dead.txt")
    fs.writeFileSync(stderr, DEAD)
    const section = await Effect.runPromise(
      Logged.section("sh", ["-c", `echo out; cat ${stderr} >&2; exit 1`], file, { header: "generator", tail: 600 }).pipe(
        Effect.provide(NodeServices.layer)
      )
    )
    expect(section.exit).toBe(1)
    expect(section.signal).toBeNull()
    expect(section.said).toContain("Rules.DataLoadError")
    expect(section.said).not.toContain("FiberImpl.runLoop")
    const log = fs.readFileSync(file, "utf8")
    expect(log.startsWith("earlier\n=== generator ===\n")).toBe(true)
    expect(log).toContain("FiberImpl.runLoop")
    expect(log.endsWith("=== exit 1 ===\n")).toBe(true)
  })

  it("names the signal a run died of", async () => {
    const section = await Effect.runPromise(
      Logged.section("sh", ["-c", "kill -KILL $$"], path.join(dir, "killed.log"), { header: "g", tail: 600 }).pipe(
        Effect.provide(NodeServices.layer)
      )
    )
    expect(section).toMatchObject({ exit: null, signal: "SIGKILL" })
  })
})
