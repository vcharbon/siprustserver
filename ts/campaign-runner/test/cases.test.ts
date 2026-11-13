/**
 * The compile loop over a fixture pack's case compiler: every selected file
 * compiles into one cell per sub-case written under the run's cells, each
 * keeping its file's text and the tags the pack states, its checks ending with
 * the pack's gates, a case source's cells' as an authored case's; a file the compiler
 * refuses is broken after the selection's own, the others still compile; two
 * sub-cases compiling to one cell id fail the phase naming both; a run keeping
 * one case compiles only it, and fails where no file compiles it. A case
 * stating an operation as a step of its call compiles to a cell running alone
 * and asking for it where a tag it carries states alone, and is broken where
 * none does; a source cell's own tags are its cell's, an undeclared one
 * breaking its source.
 */
import { NodeServices } from "@effect/platform-node"
import { type Pivot, Postcondition } from "@sip/contracts"
import type { CompiledCell } from "@sip/rig"
import type { Declared } from "@sip/workspace/campaign-tags"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import type { CaseFile, Selected } from "../src/case-file.js"
import { type CaseCompiler, type Compiled, compileCases } from "../src/cases.js"
import type { Cell, Checked } from "../src/cell.js"
import { CLEANUP_GATE, RFC_GATE } from "../src/checked.js"
import { cellOfSource } from "../src/source-cells.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "runner-cases-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** A document whose one call's b-leg lands on `bob`. */
const DOCUMENT = {
  pivot_version: 3,
  case: { id: "x", lanes: {}, origin: "test" },
  endpoints: [{ id: "alice" }, { id: "bob" }],
  actors: [{ id: "uac", kind: "uac", endpoint: "alice" }, { id: "uas", kind: "uas", endpoint: "bob" }],
  legs: [{ id: "A", dir: "out", actor: "uac" }, { id: "B", dir: "in", actor: "uas" }],
  calls: [{ id: "c1", caller_leg: "A", attempts: [{ leg: "B" }] }],
  identities: [],
  flow: []
} as unknown as Pivot.PivotV3

/** A case file at `name` under the run's case root, its text `text`. */
const file = (name: string, text = `ref: "cases/${name}"\n`): CaseFile => {
  const absolute = path.join(dir, "cases", `${name}.ts`)
  fs.mkdirSync(path.dirname(absolute), { recursive: true })
  fs.writeFileSync(absolute, text)
  return { file: `cases/${name}.ts`, absolute, value: name }
}

const selectionOf = (files: ReadonlyArray<CaseFile>): Selected => ({
  selected: files,
  broken: [{ file: "cases/unloaded.ts", absolute: path.join(dir, "cases/unloaded.ts"), reason: "does not load" }],
  partial: false,
  inCampaign: files.length + 1,
  files
})

/** One sub-case `subcase` of the case `ref`, its cell id `id`. */
const subcase = (ref: string, sub: string, id = `${ref.split("/").at(-1)}-${sub}`): Compiled => ({
  id,
  ref,
  subcase: sub,
  retries: 0,
  heal: { healMs: 0, healRealClockOnSimulatedMs: 0 },
  locks: [],
  needs: [],
  document: DOCUMENT
})

/** The flow of a document whose callee receives the INVITE (`s1`), then the operation `restart-worker` is injected. */
const STEPPED_FLOW = [
  {
    id: "s1",
    leg: "B",
    op: "expect",
    check: "assert",
    msg: { method: "INVITE" },
    delay: { ms: 0, from: "trigger", compressible: true, timer_linked: false }
  },
  { id: "i1", op: "inject", action: "operation:restart-worker", after: ["s1"] }
]

/** What a source cell states beside its id: its tags, and whether its document injects `restart-worker` as a step. */
interface SourceOptions {
  readonly tags?: ReadonlyArray<string>
  readonly stepped?: boolean
}

/** A case source's file at `name` yielding the one cell `id`, its `call-records` claim beside its document. */
const sourceFile = (name: string, id: string, options: SourceOptions = {}): CaseFile => {
  const cell: CompiledCell.CompiledCell = {
    id,
    case: `Source/${name}`,
    title: name,
    document: {
      ...DOCUMENT,
      case: { id, title: name, family: "transparent", variant: "target", origin: "authored", lanes: { "*": "ok" } },
      endpoints: [
        { id: "alice", observed: "192.0.2.10:5060", side: "peer", binding: "dedicated" },
        { id: "bob", observed: "192.0.2.20:5060", side: "peer", binding: "dedicated" }
      ],
      calls: [{ id: "c1", caller_leg: "A", attempts: [{ branch: 0, position: 0, leg: "B", callee: { identity: "bob" } }] }],
      flow: options.stepped === true ? STEPPED_FLOW : [],
      timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
    } as unknown as Pivot.PivotV3,
    resources: {},
    claims: { [Postcondition.CALL_RECORDS]: { count: 1 } },
    ...(options.tags === undefined ? {} : { tags: options.tags })
  }
  return { ...file(name), cells: [cell] }
}

/** The pack's cell: the runner's, with the pack's compiled sub-case beside it. */
interface PackCell extends Cell {
  readonly packOf: string
}

/** A gate of the pack's SUT, run between the runner's two. */
const DRAINED: Checked = { step: null, anchor: "queue drain", field: "queue.drain", op: "drained", expected: "no message left" }

/** The pack's gates, in its order. */
const GATES = [CLEANUP_GATE, DRAINED, RFC_GATE]

/**
 * A pack compiling each file by its value: `two` to sub-cases `a` and `b`,
 * `refused` refused, `clash` to a cell id `two`'s `a` takes; its cells written
 * as a directory each under the cells root, every case tagged `pack`; a case
 * source's cells are the runner's cells of them.
 */
const COMPILER: CaseCompiler<PackCell, Compiled, Selected, never> = {
  compile: (one) =>
    one.value === "refused"
      ? Effect.fail({ message: "the pack refuses this case" })
      : one.value === "stepped"
      ? Effect.succeed([{ ...subcase("cases/stepped", "a"), document: { ...DOCUMENT, flow: STEPPED_FLOW } as unknown as Pivot.PivotV3 }])
      : one.value === "clash"
      ? Effect.succeed([subcase("cases/clash", "a", "two-a")])
      : Effect.succeed([subcase(`cases/${String(one.value)}`, "a"), subcase(`cases/${String(one.value)}`, "b")]),
  linted: () => false,
  write: (root, compiled) =>
    Effect.sync(() => {
      const cell = path.join(root, compiled.id)
      fs.mkdirSync(cell, { recursive: true })
      return cell
    }),
  statedTags: () => ["pack"],
  gates: GATES,
  claimsOf: () => ({}),
  cellOf: (compiled, cell) => ({ ...cell, packOf: compiled.id }),
  writeSource: (root, compiled) =>
    Effect.sync(() => {
      const cell = path.join(root, compiled.id)
      fs.mkdirSync(cell, { recursive: true })
      return cell
    }),
  cellOfSource: (answered, placing) => ({ ...cellOfSource(answered, placing), packOf: answered.compiled.id })
}

/** The tags a deployment declares: `disruptive`, whose cases run alone, and `slow`, whose cases share their lane. */
const DECLARED: Declared = {
  file: "tool.toml",
  tags: {
    disruptive: { label: "disruptive", default: "run", alone: true },
    slow: { label: "slow", default: "run" }
  },
  tagCases: []
}

/** A compile context whose lint finds nothing in any document, the deployment declaring `declared`. */
const contextOf = (declared: Declared = { file: "tool.toml", tags: {}, tagCases: [] }) => ({
  lint: () => Effect.succeed([]),
  declared
})

const compile = (
  files: ReadonlyArray<CaseFile>,
  only: { readonly ref: string; readonly subcase: string | null } | null = null,
  declared?: Declared
) =>
  Effect.runPromise(
    Effect.result(compileCases(COMPILER, selectionOf(files), path.join(dir, "cells"), only, contextOf(declared))).pipe(
      Effect.provide(NodeServices.layer)
    )
  )

/** Each broken file of `files` compiled, the selection's own aside, as `file: reason`. */
const brokenOf = async (files: ReadonlyArray<CaseFile>, declared: Declared = DECLARED) => {
  const compiled = await compile(files, null, declared)
  if (compiled._tag !== "Success") throw new Error(String(compiled.failure))
  return compiled.success.broken.slice(1).map((one) => `${one.file}: ${one.reason}`)
}

describe("the compile loop over a pack's compiler", () => {
  it("compiles each sub-case into a cell under the cells root, a refused file broken after the selection's own", async () => {
    const compiled = await compile([file("two"), file("refused")])
    if (compiled._tag !== "Success") throw new Error(String(compiled.failure))
    expect(compiled.success.cells.map((cell) => [cell.id, cell.ref, cell.subcase, cell.dir, cell.tags, cell.egress, cell.packOf]))
      .toEqual([
        ["two-a", "cases/two", "a", path.join(dir, "cells", "two-a"), ["pack"], "bob", "two-a"],
        ["two-b", "cases/two", "b", path.join(dir, "cells", "two-b"), ["pack"], "bob", "two-b"]
      ])
    expect(compiled.success.cells[0]!.source).toEqual({ file: "cases/two.ts", text: `ref: "cases/two"\n` })
    expect(compiled.success.broken.map((one) => [one.file, one.reason])).toEqual([
      ["cases/unloaded.ts", "does not load"],
      ["cases/refused.ts", "the pack refuses this case"]
    ])
  })

  it("ends each cell's checks with the gates the pack's compiler states, in its order", async () => {
    const compiled = await compile([file("two")])
    if (compiled._tag !== "Success") throw new Error(String(compiled.failure))
    expect(compiled.success.cells.map((cell) => cell.checked.map((one) => one.anchor))).toEqual([
      ["post-run cleanup", "queue drain", "rfc"],
      ["post-run cleanup", "queue drain", "rfc"]
    ])
  })

  it("ends a case source's cell's checks with the pack's gates, in its order", async () => {
    const compiled = await compile([sourceFile("yielding", "yielded-1")])
    if (compiled._tag !== "Success") throw new Error(String(compiled.failure))
    expect(compiled.success.cells.map((cell) => [cell.id, cell.packOf, cell.checked.map((one) => one.anchor)])).toEqual([
      ["yielded-1", "yielded-1", ["post-run cleanup", "queue drain", "rfc"]]
    ])
  })

  it("fails the phase where two sub-cases compile to one cell id, naming both", async () => {
    const compiled = await compile([file("two"), file("clash")])
    expect(compiled._tag).toBe("Failure")
    expect(compiled._tag === "Failure" ? compiled.failure.message : "").toContain(
      "two sub-cases compile to the cell two-a: cases/two [a] (cases/two.ts) and cases/clash [a] (cases/clash.ts)"
    )
  })

  it("compiles only the case a run keeps, and fails where no selected file compiles it", async () => {
    const kept = await compile([file("two")], { ref: "cases/two", subcase: "b" })
    expect(kept._tag === "Success" ? kept.success.cells.map((cell) => cell.id) : kept.failure).toEqual(["two-b"])
    const none = await compile([file("two")], { ref: "cases/gone", subcase: null })
    expect(none._tag === "Failure" ? none.failure.message : "").toContain("no selected case file compiles cases/gone")
  })
})

describe("a case stating an operation as a step of its call", () => {
  it("compiles where a tag it carries states alone: its cell runs alone and asks for the operation", async () => {
    const compiled = await compile([sourceFile("restarting", "restarting-1", { stepped: true, tags: ["disruptive"] })], null, DECLARED)
    if (compiled._tag !== "Success") throw new Error(String(compiled.failure))
    expect(compiled.success.broken.slice(1)).toEqual([])
    expect(compiled.success.cells.map((cell) => [cell.id, cell.tags, cell.alone, cell.operations])).toEqual([
      ["restarting-1", ["disruptive"], true, ["restart-worker"]]
    ])
  })

  it("is refused at compile where no tag it carries states alone, naming the cell and the operation", async () => {
    const refusal = "restarting-1: injects restart-worker as a step of its call, and carries no tag stating alone: " +
      "other cells on its lane would run through it"
    expect(await brokenOf([sourceFile("restarting", "restarting-1", { stepped: true, tags: ["slow"] })])).toEqual([
      `cases/restarting.ts: ${refusal}`
    ])
    expect(await brokenOf([sourceFile("restarting", "restarting-1", { stepped: true })])).toEqual([`cases/restarting.ts: ${refusal}`])
  })

  it("is refused at compile in a pack's own case file too, its tags the file's", async () => {
    expect(await brokenOf([file("stepped")])).toEqual([
      "cases/stepped.ts: stepped-a: injects restart-worker as a step of its call, and carries no tag stating alone: " +
      "other cells on its lane would run through it"
    ])
    const alone = await compile([file("stepped")], null, { ...DECLARED, tags: { pack: { label: "pack", default: "run", alone: true } } })
    expect(alone._tag === "Success" ? alone.success.cells.map((cell) => [cell.id, cell.alone, cell.operations]) : alone.failure)
      .toEqual([["stepped-a", true, ["restart-worker"]]])
  })
})

describe("a case source's cell carrying tags", () => {
  it("runs alone where one states so, with no step of its own", async () => {
    const compiled = await compile([sourceFile("quiet", "quiet-1", { tags: ["disruptive", "slow"] })], null, DECLARED)
    expect(compiled._tag === "Success" ? compiled.success.cells.map((cell) => [cell.tags, cell.alone, cell.operations]) : compiled.failure)
      .toEqual([[["disruptive", "slow"], true, undefined]])
  })

  it("is refused at compile where one is no tag the deployment declares, naming it", async () => {
    expect(await brokenOf([sourceFile("odd", "odd-1", { tags: ["flaky"] })])).toEqual([
      "cases/odd.ts: odd-1: names flaky, no declared tag (declared: disruptive, slow)"
    ])
  })
})
