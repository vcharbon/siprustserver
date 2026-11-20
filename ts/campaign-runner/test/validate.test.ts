/**
 * A campaign validated over the run's compile phase and a fixture pack's
 * compiler, its lint answering by document: a valid set has no problem and
 * exits 0; a file the compiler refuses, and two sub-cases compiling to one
 * cell id, are compile problems; a compiled document the lint finds an error
 * in is a schema problem named by its file and sub-case; two cells filed under
 * one case id are a consistency problem; a lane timer and a cell no lane runs
 * are notes, never problems. With `--out`, the cells' descriptions, the
 * problems and each cell's expanded expectation are written, every step field
 * and check class the document states among it; a reused `--out` keeps only
 * its last validation's cells, and one holding other files is refused.
 */
import { NodeServices } from "@effect/platform-node"
import type { Pivot } from "@sip/contracts"
import { LaneView } from "@sip/rig"
import type { Declared } from "@sip/workspace/campaign-tags"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import type { CaseFile, Selected } from "../src/case-file.js"
import { type CaseCompiler, type Compiled, compileCases } from "../src/cases.js"
import type { Cell } from "../src/cell.js"
import { CLEANUP_GATE, RFC_GATE } from "../src/checked.js"
import { readExpanded } from "../src/expanded.js"
import type { Lint } from "../src/lint.js"
import { cellOfSource } from "../src/source-cells.js"
import { exitOf, outRefusal, problemLines, validate, type Validation, withCellsDir, writeOut } from "../src/validate.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "runner-validate-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/**
 * Alice's INVITE, sent once the lane is ready, reaching Bob, who answers it
 * 200 confirming the dialog, with the To tag checked.
 */
const FLOW = [
  {
    id: "s1",
    leg: "A",
    op: "send",
    awaits: "lane-ready",
    msg: {
      method: "INVITE",
      cseq: 1,
      contact: { display: "Alice", user: "alice" },
      headers: [{ name: "Allow", value: "INVITE, ACK, BYE" }, { name: "X-Origin", value: "a", class: "origin-platform-header" }]
    },
    delay: { ms: 0, from: "trigger", compressible: true, timer_linked: false }
  },
  {
    id: "s2",
    leg: "B",
    op: "expect",
    check: "assert",
    msg: { method: "INVITE" },
    checks: [
      { field: "hdr.From", op: "regex", value: "alice" },
      { field: "hdr.X-Debug", op: "absent" },
      { field: "hdr.X-Origin", op: "exists", class: "origin-platform-header" }
    ],
    delay: { ms: 0, from: "step:s1", compressible: true, timer_linked: false }
  },
  {
    id: "s3",
    leg: "A",
    op: "expect",
    check: "assert",
    confirms_dialog: true,
    after: ["s2"],
    msg: { status: 200, reason: "OK", "cseq-method": "INVITE" },
    checks: [{ field: "status", op: "eq", value: "200" }],
    delay: { ms: 32000, from: "step:s2", compressible: false, timer_linked: true }
  }
]

/** A document whose one call's b-leg lands on `bob`, its flow {@link FLOW}. */
const DOCUMENT = {
  pivot_version: 3,
  case: { id: "x", title: "alice calls bob", lanes: { "*": "ok" }, origin: "authored", family: "transparent", variant: "target" },
  endpoints: [{ id: "alice", observed: "192.0.2.10:5060", side: "peer", binding: "dedicated" }, {
    id: "bob",
    observed: "192.0.2.20:5060",
    side: "peer",
    binding: "dedicated"
  }],
  actors: [{ id: "uac", kind: "uac", endpoint: "alice" }, { id: "uas", kind: "uas", endpoint: "bob" }],
  legs: [{ id: "A", dir: "out", actor: "uac" }, { id: "B", dir: "in", actor: "uas" }],
  calls: [{ id: "c1", caller_leg: "A", attempts: [{ branch: 0, position: 0, leg: "B", callee: { identity: "bob" } }] }],
  identities: [],
  flow: FLOW,
  postconditions: { cdr: { count: 1 } },
  timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
} as unknown as Pivot.PivotV3

/** A case file at `name`, its default export `value`. */
const file = (name: string, value: string = name): CaseFile => {
  const absolute = path.join(dir, "cases", `${name}.ts`)
  fs.mkdirSync(path.dirname(absolute), { recursive: true })
  fs.writeFileSync(absolute, `ref: "cases/${name}"\n`)
  return { file: `cases/${name}.ts`, absolute, value }
}

const selectionOf = (files: ReadonlyArray<CaseFile>): Selected => ({ selected: files, broken: [], partial: false, inCampaign: files.length, files })

/** The sub-case `sub` of the case `ref`, its cell id `id`. */
const subcase = (ref: string, sub: string, id: string, over: Partial<Compiled> = {}): Compiled => ({
  id,
  ref,
  subcase: sub,
  retries: 0,
  heal: { healMs: 0, healRealClockOnSimulatedMs: 0 },
  locks: [],
  needs: [],
  document: DOCUMENT,
  ...over
})

/**
 * A pack compiling each file by its value: `refused` refused, `clash` to the
 * cell id `ok`'s `a` takes, `twin` to `ok`'s case id under another cell id,
 * `keepalive` with a lane timer, `video` needing a capability, any other to
 * sub-cases `a` and `b`; each document written as its cell's `scenario.json`.
 */
const COMPILER: CaseCompiler<Cell, Compiled, Selected, never> = {
  compile: (one) => {
    const name = String(one.value)
    if (name === "refused") return Effect.fail({ message: "the pack refuses this case" })
    if (name === "clash") return Effect.succeed([subcase("cases/clash", "a", "ok-a")])
    if (name === "twin") return Effect.succeed([subcase("cases/ok", "a", "twin-a")])
    if (name === "keepalive") return Effect.succeed([subcase("cases/keepalive", "a", "keepalive-a", { laneTimers: { keepalive: 30000 } })])
    if (name === "video") return Effect.succeed([subcase("cases/video", "a", "video-a", { needs: ["video"] })])
    return Effect.succeed([subcase(`cases/${name}`, "a", `${name}-a`), subcase(`cases/${name}`, "b", `${name}-b`)])
  },
  linted: () => false,
  write: (root, compiled) =>
    Effect.sync(() => {
      const cell = path.join(root, compiled.id)
      fs.mkdirSync(cell, { recursive: true })
      fs.writeFileSync(path.join(cell, "scenario.json"), JSON.stringify(compiled.document))
      return cell
    }),
  statedTags: () => [],
  gates: [CLEANUP_GATE, RFC_GATE],
  claimsOf: () => ({}),
  cellOf: (_compiled, cell) => cell,
  writeSource: () => Effect.fail({ message: "no source here" }),
  cellOfSource
}

const DECLARED: Declared = { file: "tool.toml", tags: {}, tagCases: [] }

/** A lint finding an error in the document of every cell directory named `bad-…`, none elsewhere. */
const LINT: Lint = (scenario) =>
  Effect.succeed(
    path.basename(path.dirname(scenario)).startsWith("bad-")
      ? ["ref/after-unknown at flow[id:s3]: `after` names s9, which no flow node is"]
      : []
  )

/** The catalog's one lane: the simulated one, capable of `sip` alone. */
const LANE: LaneView.View = { ...LaneView.simulatedBuiltIn("sim", {}), capabilities: new Set(["sip"]) }

/** The validation of `files` compiled under `out` (a scratch directory where none is named), and what was written under `out`. */
const validated = (files: ReadonlyArray<CaseFile>, out?: string) =>
  Effect.runPromise(
    withCellsDir(out, (cellsDir) =>
      Effect.gen(function*() {
        const validation = yield* validate(compileCases(COMPILER, selectionOf(files), cellsDir, null, { lint: LINT, declared: DECLARED }), {
          lint: LINT,
          lanes: [LANE]
        })
        if (out !== undefined) yield* writeOut(out, validation, (cell) => readExpanded(cell))
        return validation
      })).pipe(Effect.provide(NodeServices.layer))
  )

const linesOf = (validation: Validation) => ({ lines: problemLines(validation), exit: exitOf(validation) })

describe("a campaign validated with no lane", () => {
  it("finds no problem in a valid set and exits 0", async () => {
    const validation = await validated([file("ok"), file("other")])
    expect(validation.cells.map((cell) => cell.id)).toEqual(["ok-a", "ok-b", "other-a", "other-b"])
    expect(linesOf(validation)).toEqual({ lines: [], exit: 0 })
    expect(validation.notes).toEqual([])
  })

  it("names a file the compile refuses as a compile problem, the others still validated", async () => {
    expect(linesOf(await validated([file("ok"), file("refused")]))).toEqual({
      lines: ["compile cases/refused.ts: the pack refuses this case"],
      exit: 1
    })
  })

  it("names the compile phase failing on two sub-cases compiling to one cell id", async () => {
    expect(linesOf(await validated([file("ok"), file("clash")]))).toEqual({
      lines: ["compile (the compile phase): compile: two sub-cases compile to the cell ok-a: cases/ok [a] (cases/ok.ts) and cases/clash [a] (cases/clash.ts)"],
      exit: 1
    })
  })

  it("names a compiled document the lint finds an error in as a schema problem of its file and sub-case", async () => {
    expect(linesOf(await validated([file("ok"), file("bad")]))).toEqual({
      lines: [
        "schema cases/bad.ts [a]: ref/after-unknown at flow[id:s3]: `after` names s9, which no flow node is",
        "schema cases/bad.ts [b]: ref/after-unknown at flow[id:s3]: `after` names s9, which no flow node is"
      ],
      exit: 1
    })
  })

  it("names two cells filed under one case id as a consistency problem", async () => {
    expect(linesOf(await validated([file("ok"), file("twin")]))).toEqual({
      lines: [
        "consistency cases/twin.ts [a]: the cell twin-a is filed under the case id cases/ok [a], as the cell ok-a of cases/ok.ts is: " +
        "the store keys a cell's results by it"
      ],
      exit: 1
    })
  })

  it("notes a lane timer as resolved at run, and a cell no lane runs, with no problem", async () => {
    const validation = await validated([file("keepalive"), file("video")])
    expect(linesOf(validation)).toEqual({ lines: [], exit: 0 })
    expect(validation.notes).toEqual([
      "cases/keepalive.ts: cases/keepalive [a]: lane timer keepalive compiled as 30000 ms, resolved at run over the run's lanes",
      "cases/video.ts: cases/video [a]: runs on no lane, skipped at run: not-runnable: the case cases/video is not runnable on this lane: sim lacks video"
    ])
  })

  it("writes under --out each compiled document, the cells' descriptions, the problems and each cell's expanded expectation", async () => {
    const out = path.join(dir, "out")
    await validated([file("ok"), file("bad")], out)
    expect(fs.existsSync(path.join(out, "cells", "ok-a", "scenario.json"))).toBe(true)
    const described = JSON.parse(fs.readFileSync(path.join(out, "cells.json"), "utf8")) as ReadonlyArray<Record<string, unknown>>
    expect(described.map((cell) => [cell["id"], cell["file"], cell["source"]])).toEqual([
      ["ok-a", "cases/ok.ts", undefined],
      ["ok-b", "cases/ok.ts", undefined],
      ["bad-a", "cases/bad.ts", undefined],
      ["bad-b", "cases/bad.ts", undefined]
    ])
    const written = JSON.parse(fs.readFileSync(path.join(out, "validation.json"), "utf8")) as { problems: ReadonlyArray<{ level: string }> }
    expect(written.problems.map((p) => p.level)).toEqual(["schema", "schema"])
    const expanded = JSON.parse(fs.readFileSync(path.join(out, "cells", "ok-a", "expanded.json"), "utf8"))
    expect(expanded.unreadable).toBeUndefined()
    expect(expanded.legs.A.messages.map((m: { message: string }) => m.message)).toEqual(["INVITE", "200 OK"])
    expect(expanded.legs.A.messages[0]).toMatchObject({
      awaits: "lane-ready",
      stated: {
        cseq: 1,
        contact: { display: "Alice", user: "alice" },
        headers: [{ name: "Allow", value: "INVITE, ACK, BYE" }, { name: "X-Origin", value: "a", class: "origin-platform-header" }]
      }
    })
    expect(expanded.legs.A.messages[1].confirms_dialog).toBe(true)
    expect(expanded.legs.B.messages[0].checks).toEqual({
      exact: [],
      pattern: [{ field: "hdr.From", value: "alice" }],
      present: [{ field: "hdr.X-Origin", class: "origin-platform-header" }],
      absent: [{ field: "hdr.X-Debug" }]
    })
    expect(expanded.graph.nodes.map((n: { id: string; after: ReadonlyArray<string> }) => [n.id, n.after])).toEqual([
      ["s1", []],
      ["s2", []],
      ["s3", ["s2"]]
    ])
    expect(expanded.timers.linkedDelays.map((d: { step: string; ms: number }) => [d.step, d.ms])).toEqual([["s3", 32000]])
    expect(expanded.claims.postconditions).toEqual({ cdr: { count: 1 } })
    expect(expanded.claims.checked.at(-1).anchor).toBe("rfc")
  })

  it("leaves under a reused --out only the cells of its last validation", async () => {
    const out = path.join(dir, "again")
    await validated([file("ok"), file("other")], out)
    await validated([file("ok")], out)
    expect(fs.readdirSync(path.join(out, "cells")).sort()).toEqual(["ok-a", "ok-b"])
  })

  it("refuses an --out holding what no validation writes, and takes an empty or absent one", async () => {
    const foreign = path.join(dir, "foreign")
    fs.mkdirSync(foreign, { recursive: true })
    fs.writeFileSync(path.join(foreign, "notes.txt"), "mine\n")
    const refusalAt = (out: string) => Effect.runPromise(outRefusal(out).pipe(Effect.provide(NodeServices.layer)))
    expect(await refusalAt(foreign)).toBe(
      `--out ${foreign} holds notes.txt, which a validation does not write: name an empty directory, or one a validation wrote`
    )
    expect(await refusalAt(path.join(dir, "absent"))).toBeUndefined()
    await validated([file("ok")], path.join(dir, "again"))
    expect(await refusalAt(path.join(dir, "again"))).toBeUndefined()
  })
})
