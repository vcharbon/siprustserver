/**
 * The compile loop over a fixture pack's case compiler: every selected file
 * compiles into one cell per sub-case written under the run's cells, each
 * keeping its file's text and the tags the pack states, its checks ending with
 * the pack's gates, a case source's cells' as an authored case's; a file the compiler
 * refuses is broken after the selection's own, the others still compile; two
 * sub-cases compiling to one cell id fail the phase naming both; a run keeping
 * one case compiles only it, and fails where no file compiles it.
 */
import { NodeServices } from "@effect/platform-node"
import { type Pivot, Postcondition } from "@sip/contracts"
import type { CompiledCell } from "@sip/rig"
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
const file = (name: string, text = `ref: "Routing/${name}"\n`): CaseFile => {
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

/** A case source's file at `name` yielding the one cell `id`, its `call-records` claim beside its document. */
const sourceFile = (name: string, id: string): CaseFile => {
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
      timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
    } as unknown as Pivot.PivotV3,
    resources: {},
    claims: { [Postcondition.CALL_RECORDS]: { count: 1 } }
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
      : one.value === "clash"
      ? Effect.succeed([subcase("Routing/clash", "a", "two-a")])
      : Effect.succeed([subcase(`Routing/${String(one.value)}`, "a"), subcase(`Routing/${String(one.value)}`, "b")]),
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

/** A compile context whose lint finds nothing in any document. */
const CONTEXT = {
  lint: () => Effect.succeed([]),
  declared: { file: "tool.toml", tags: {}, tagCases: [] }
}

const compile = (files: ReadonlyArray<CaseFile>, only: { readonly ref: string; readonly subcase: string | null } | null = null) =>
  Effect.runPromise(
    Effect.result(compileCases(COMPILER, selectionOf(files), path.join(dir, "cells"), only, CONTEXT)).pipe(
      Effect.provide(NodeServices.layer)
    )
  )

describe("the compile loop over a pack's compiler", () => {
  it("compiles each sub-case into a cell under the cells root, a refused file broken after the selection's own", async () => {
    const compiled = await compile([file("two"), file("refused")])
    if (compiled._tag !== "Success") throw new Error(String(compiled.failure))
    expect(compiled.success.cells.map((cell) => [cell.id, cell.ref, cell.subcase, cell.dir, cell.tags, cell.egress, cell.packOf]))
      .toEqual([
        ["two-a", "Routing/two", "a", path.join(dir, "cells", "two-a"), ["pack"], "bob", "two-a"],
        ["two-b", "Routing/two", "b", path.join(dir, "cells", "two-b"), ["pack"], "bob", "two-b"]
      ])
    expect(compiled.success.cells[0]!.source).toEqual({ file: "cases/two.ts", text: `ref: "Routing/two"\n` })
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
      "two sub-cases compile to the cell two-a: Routing/two [a] (cases/two.ts) and Routing/clash [a] (cases/clash.ts)"
    )
  })

  it("compiles only the case a run keeps, and fails where no selected file compiles it", async () => {
    const kept = await compile([file("two")], { ref: "Routing/two", subcase: "b" })
    expect(kept._tag === "Success" ? kept.success.cells.map((cell) => cell.id) : kept.failure).toEqual(["two-b"])
    const none = await compile([file("two")], { ref: "Routing/gone", subcase: null })
    expect(none._tag === "Failure" ? none.failure.message : "").toContain("no selected case file compiles Routing/gone")
  })
})
