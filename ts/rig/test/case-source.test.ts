/**
 * A case source: the modules `pack.toml` names, each module's default export
 * read as compiled cells, every refusal naming the module, and a cell written
 * as its document, resources and claims.
 */
import { NodeServices } from "@effect/platform-node"
import { Pivot } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import * as CaseSource from "../src/case-source.js"
import type { CompiledCell } from "../src/compiled-cell.js"
import * as Written from "../src/compiled-cell.js"
import { pack, run, tree } from "./tree.js"

const DELAY = { ms: 0, from: "trigger", compressible: true, timer_linked: false }

/** A one-step cell whose INVITE carries the resource `ref`. */
const cellNamed = (id: string, ref = "resources/offer.sdp") => ({
  id,
  case: "a case",
  title: "a case",
  document: {
    pivot_version: 3,
    case: { id, title: "a case", family: "transparent", variant: "target", origin: "authored", lanes: { "*": "ok" } },
    identities: [],
    calls: [],
    endpoints: [{ id: "e", observed: "192.0.2.10:5060", side: "peer", binding: "dedicated" }],
    actors: [{ id: "a", kind: "uac", endpoint: "e" }],
    legs: [{ id: "A", actor: "a", dir: "out" }],
    flow: [{ id: "s1", leg: "A", op: "send", msg: { method: "INVITE", body: { ref } }, delay: DELAY }],
    timing: { expect_budget_ms: 1000, settle_budget_ms: 1000 }
  },
  resources: { "resources/offer.sdp": "v=0\r\n" },
  claims: { "call-records": { count: 1 } }
})

const refusalOf = (value: unknown): string => {
  const cells = CaseSource.cellsOf("cases.ts", value)
  return Result.isFailure(cells) ? cells.failure.message : "accepted"
}

describe("the sources a pack names", () => {
  it("are its [cases] sources resolved against its root, in its order, and none where it names none", async () => {
    const root = tree({ "pack.toml": pack(`[cases]\nsources = ["./b.ts", "./sub/a.ts"]`) })
    const named = await run(CaseSource.namedBy(root))
    expect(named).toEqual(Result.succeed([
      { path: "./b.ts", absolute: path.join(root, "b.ts") },
      { path: "./sub/a.ts", absolute: path.join(root, "sub/a.ts") }
    ]))
    expect(await run(CaseSource.namedBy(tree({ "pack.toml": pack() })))).toEqual(Result.succeed([]))
  })
})

describe("a source's default export", () => {
  it("yields its cells, each document decoded as pivot v3", () => {
    const cells = CaseSource.cellsOf("cases.ts", [cellNamed("one"), cellNamed("two")])
    expect(Result.map(cells, (all) => all.map((cell) => [cell.id, cell.document.flow.length, cell.claims]))).toEqual(Result.succeed([
      ["one", 1, { "call-records": { count: 1 } }],
      ["two", 1, { "call-records": { count: 1 } }]
    ]))
  })

  it("is refused when it is no array", () => {
    expect(refusalOf({ cells: [] })).toBe("cases.ts: its default export is no array of compiled cells")
  })

  it("is refused when a cell does not decode, naming its index", () => {
    const { document, ...rest } = cellNamed("one")
    expect(refusalOf([cellNamed("ok"), { ...rest, document: { ...document, extra: true } }])).toMatch(/^cases\.ts: cell 1: /)
    expect(refusalOf([{ ...cellNamed("one"), id: "Not A Name" }])).toMatch(/^cases\.ts: cell 0: id: /)
  })

  it("is refused when a body names a resource the cell does not carry", () => {
    expect(refusalOf([cellNamed("one", "resources/missing.sdp")])).toBe(
      "cases.ts: cell one: its document names the resource resources/missing.sdp, which the cell does not carry"
    )
  })

  it("is refused when two cells state one id", () => {
    expect(refusalOf([cellNamed("one"), cellNamed("one")])).toBe("cases.ts: two cells state the id one")
  })
})

describe("a source module", () => {
  it("is read by its default export, and refused naming it where it does not load", async () => {
    const root = tree({
      "good.mjs": `export default ${JSON.stringify([cellNamed("one")])}\n`,
      "bad.mjs": `throw new Error("broken source")\n`
    })
    const good = await run(CaseSource.load(path.join(root, "good.mjs")))
    expect(Result.map(good, (cells) => cells.map((cell) => cell.id))).toEqual(Result.succeed(["one"]))
    const bad = await run(CaseSource.load(path.join(root, "bad.mjs")))
    expect(Result.isFailure(bad) ? bad.failure.message : "loaded").toBe(`${path.join(root, "bad.mjs")}: does not load: broken source`)
  })
})

describe("a written cell", () => {
  it("is its directory holding the canonical document, its resources and its claims", async () => {
    const cells = CaseSource.cellsOf("cases.ts", [cellNamed("one")])
    const cell: CompiledCell = Result.getOrThrow(cells)[0]!
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rig-cell-"))
    const written = await Effect.runPromise(Written.write(dir, cell).pipe(Effect.provide(NodeServices.layer)))
    expect(written).toBe(path.join(dir, "one"))
    expect(fs.readFileSync(path.join(written, "scenario.json"), "utf8")).toBe(Pivot.emitPivot(cell.document))
    expect(fs.readFileSync(path.join(written, "resources/offer.sdp"), "utf8")).toBe("v=0\r\n")
    expect(JSON.parse(fs.readFileSync(path.join(written, "claims.json"), "utf8"))).toEqual({ "call-records": { count: 1 } })
  })
})
