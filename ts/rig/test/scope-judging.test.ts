/**
 * A scope's records joined and judged, its collectors and judges run as real
 * executables and modules: a collector's 1000 records joined to the cells
 * whose Call-ID they carry, written in each cell's directory; a record two
 * cells' keys hold reaching the judge as ambiguous; a module judge and an
 * executable judge answering the same lines on the same input; a later
 * collector reading the keys and records an earlier one added; exports naming
 * a cell by its directory read as that cell, an unreadable line skipped and
 * its name `not checkable` where it was; a judge that fails in any way (exits
 * non-zero, returns no lines, dies, rejects, answers what is not its, times
 * out), a collector whose open or collect failed, its judge not run over its
 * cells (those of its other instances judged), and a verification nobody
 * answered, `not checkable` (the judge's own informative line answers it);
 * an executable
 * judge's lines read on its stdout and in its `lines.jsonl`.
 */
import { Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import { createRequire } from "node:module"
import * as path from "node:path"
import { pathToFileURL } from "node:url"
import { afterEach, describe, expect, it } from "vitest"
import type * as Cell from "../src/cell.js"
import * as CollectorPlan from "../src/collector-plan.js"
import * as CollectorRun from "../src/collector-run.js"
import * as JudgePlan from "../src/judge-plan.js"
import type * as Lanes from "../src/lanes.js"
import * as RigPath from "../src/rig-path.js"
import { cellOf, memoryLocker } from "./collectors.js"
import { laneOf } from "./operations.js"
import { type Entry, run, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const LANE = laneOf("lane-a", {})

const node = (body: string): Entry => ({
  text: `#!/usr/bin/env node\nconst fs = require("node:fs")\nconst ctx = JSON.parse(fs.readFileSync(process.env.SIPRIG_CONTEXT, "utf8"))\n${body}\n`,
  executable: true
})

/** The files of a batch collector `name`: its manifest (`extra` lines before `[run]`) and its collect. */
const collector = (name: string, collect: Entry, extra = ""): Record<string, Entry> => ({
  [`collectors/${name}/collector.toml`]: `schema = "collector.v1"\nname = "${name}"\nscope = "batch"\n${extra}\n[run]\ncollect = "./collect"\n`,
  [`collectors/${name}/collect`]: collect
})

/** The files of an executable judge `name`. */
const executableJudge = (name: string, body: Entry): Record<string, Entry> => ({
  [`judges/${name}/judge.toml`]: `schema = "judge.v1"\nname = "${name}"\nrun = "./judge"\n`,
  [`judges/${name}/judge`]: body
})

/** The files of a module judge `name`, `extra` lines ending its manifest. */
const moduleJudge = (name: string, text: string, extra = ""): Record<string, Entry> => ({
  [`judges/${name}/judge.toml`]: `schema = "judge.v1"\nname = "${name}"\nmodule = "./judge.ts"\n${extra}\n`,
  [`judges/${name}/judge.ts`]: text
})

/** The `effect/Effect` module the rig runs on, as a URL a module judge in a temporary directory imports. */
const EFFECT_URL = pathToFileURL(createRequire(import.meta.url).resolve("effect/Effect")).href

/** The rig path of `files`, loaded; its root. */
const loaded = async (files: Readonly<Record<string, Entry>>) => {
  const root = tree(files)
  made.push(root)
  const read = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(read)) throw new Error(`the path does not load: ${String(read.failure)}`)
  return { root, rig: read.success }
}

/** The cells `ids` of {@link LANE}, their directories under `root`'s `cells/` named as `dirs` says, `edit` applied to each. */
const cellsOf = (
  root: string,
  ids: ReadonlyArray<string>,
  edit: (cell: Cell.Cell) => Cell.Cell = (cell) => cell,
  dirs: Readonly<Record<string, string>> = {}
): ReadonlyArray<Cell.Cell> => ids.map((id) => edit(cellOf(id, LANE.id, path.join(root, "cells", dirs[id] ?? id))))

/** The batch's collectors `rig` plans on {@link LANE} under `root`, collected over `cells` and `undescribed`, the batch's export judges with them where `exports` says. */
const collectBatch = async (
  rig: RigPath.RigPath,
  root: string,
  cells: ReadonlyArray<Cell.Cell>,
  exports = false,
  undescribed: ReadonlyArray<CollectorRun.Unread> = []
) => {
  const holder = path.join(root, "batch")
  const instances = CollectorPlan.batchPlanOf(rig, LANE, path.join(holder, "collectors"), holder)
  const said: Array<string> = []
  const options: CollectorRun.Options = {
    sink: (event) => Effect.sync(() => void said.push(`${event.phase} ${event.collector}${event.message === undefined ? "" : `: ${event.message}`}`)),
    locker: memoryLocker(),
    ...(exports ? { exportJudges: JudgePlan.exportJudgesOn(rig, LANE, holder) } : {})
  }
  const collected = await Effect.runPromise(Effect.scoped(Effect.flatMap(CollectorRun.opened(instances, options), (opened) => opened.collect({ cells, unread: undescribed, failed: [] }))))
  return { collected, said, holder }
}

/** The job's collectors `rig` plans over `lanes` under `root`, collected over `cells`. */
const collectJob = (rig: RigPath.RigPath, root: string, lanes: ReadonlyArray<Lanes.Loaded>, cells: ReadonlyArray<Cell.Cell>) => {
  const holder = path.join(root, "job")
  const instances = CollectorPlan.jobPlanOf(rig, lanes, path.join(holder, "collectors"), holder)
  const options: CollectorRun.Options = { sink: () => Effect.void, locker: memoryLocker() }
  return Effect.runPromise(Effect.scoped(Effect.flatMap(CollectorRun.opened(instances, options), (opened) => opened.collect({ cells, unread: [], failed: [] }))))
}

const linesOf = (file: string): ReadonlyArray<unknown> =>
  fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "").map((line) => JSON.parse(line))

const byLabel = (collected: ReadonlyArray<CollectorRun.Collected>, label: string): CollectorRun.Collected => {
  const found = collected.find((one) => one.label === label)
  if (found === undefined) throw new Error(`no ${label} among ${collected.map((one) => one.label).join(", ")}`)
  return found
}

/** Per cell id, then for the scope, the verification lines `one` answered. */
const verdictsOf = (one: CollectorRun.Collected) => ({
  cells: Object.fromEntries([...one.cells].map(([id, answers]) => [id, answers.verifications])),
  scope: one.scope.verifications
})

/** The verification lines every entry of `collected` answered on the cell `id`. */
const onCell = (collected: ReadonlyArray<CollectorRun.Collected>, id: string) =>
  collected.flatMap((one) => one.cells.get(id)?.verifications ?? [])

/** The verification lines every entry of `collected` answered on the scope. */
const onScope = (collected: ReadonlyArray<CollectorRun.Collected>) => collected.flatMap((one) => one.scope.verifications)

/** The base's own line on a holder whose input was not read whole, saying `why`. */
const inputUnread = (why: string) =>
  expect.objectContaining({ name: "input", source: "rig", answer: "not-checkable", message: expect.stringContaining(why) })

/** Writes `lines` as the export `name` of the directory `dir`, `raw` text lines after them. */
const writeExport = (dir: string, name: string, lines: ReadonlyArray<object>, raw: ReadonlyArray<string> = []) => {
  fs.mkdirSync(path.join(dir, "exports"), { recursive: true })
  fs.writeFileSync(path.join(dir, "exports", `${name}.jsonl`), [...lines.map((line) => JSON.stringify(line)), ...raw].map((line) => `${line}\n`).join(""))
}

describe("a scope's join", () => {
  it("joins a collector's 1000 records keyed by Call-ID to the cells holding it, each cell's in its directory, the stray one kept on the scope", async () => {
    const { root, rig } = await loaded(collector(
      "calls",
      node(`
for (let i = 0; i < 999; i++) console.log(JSON.stringify({ record: { seq: i }, key: i % 2 === 0 ? "c1@test" : "c2@test" }))
console.log(JSON.stringify({ record: { seq: 999 }, key: "stray@test" }))
`),
      `[join]\nkey = "call_id"`
    ))
    const cells = cellsOf(root, ["c1", "c2"])
    const { collected, holder } = await collectBatch(rig, root, cells)
    const c1 = linesOf(path.join(root, "cells", "c1", "joined", "calls.jsonl"))
    const c2 = linesOf(path.join(root, "cells", "c2", "joined", "calls.jsonl"))
    expect([c1.length, c2.length, c1[1], c2[0]]).toEqual([500, 499, { seq: 2 }, { seq: 1 }])
    expect(linesOf(path.join(holder, "joined", "calls.unclaimed.jsonl"))).toEqual([{ record: { seq: 999 }, key: "stray@test" }])
    expect(byLabel(collected, "calls").scope.attachments).toEqual([
      { name: "calls-unclaimed", kind: "log", title: "1 unclaimed record(s) of calls", file: "joined/calls.unclaimed.jsonl", source: "calls" }
    ])
  })

  it("hands the judge a record two cells' keys hold as ambiguous, joined to neither cell", async () => {
    const { root, rig } = await loaded({
      ...collector("shared", node(`console.log(JSON.stringify({ record: { n: 1 }, key: "shared@test" }))`), `judge = "peek"\n[join]\nkey = "call_id"`),
      ...executableJudge("peek", node(`
fs.copyFileSync(process.env.SIPRIG_CONTEXT, "../../peek-context.json")
fs.copyFileSync(ctx.judging.ambiguous, "../../peek-ambiguous.jsonl")
`))
    })
    const cells = cellsOf(root, ["c1", "c2"], (cell) => ({ ...cell, keys: { ...cell.keys, call_id: [`${cell.id}@test`, "shared@test"] } }))
    await collectBatch(rig, root, cells)
    const context = JSON.parse(fs.readFileSync(path.join(root, "peek-context.json"), "utf8"))
    expect(linesOf(path.join(root, "peek-ambiguous.jsonl"))).toEqual([{ record: { n: 1 }, key: "shared@test", cells: ["c1", "c2"] }])
    expect(context.judging.name).toBe("shared")
    expect(context.cells.map((cell: Cell.Cell) => [cell.id, linesOf(cell.records.shared!)])).toEqual([["c1", []], ["c2", []]])
  })

  it("answers the same lines from a module judge and an executable judge given the same records and claims", async () => {
    const counts = collector(
      "counts",
      node(`
for (const key of ["c1@test", "c1@test", "c2@test", "nobody@test"]) console.log(JSON.stringify({ record: { key }, key }))
`),
      `judge = "counts"\nanswers = ["counts"]\n[join]\nkey = "call_id"`
    )
    const asExecutable = await loaded({
      ...counts,
      ...executableJudge("counts", node(`
const count = (file) => fs.readFileSync(file, "utf8").split("\\n").filter((line) => line !== "").length
for (const cell of ctx.cells) {
  const n = count(cell.records.counts)
  console.log(JSON.stringify({ verification: "counts", cell: cell.id, answer: n === cell.claims.counts.count ? "pass" : "fail", message: n + " record(s)" }))
}
console.log(JSON.stringify({ verification: "counts", answer: "pass", message: "unclaimed " + count(ctx.judging.unclaimed) + ", ambiguous " + count(ctx.judging.ambiguous) }))
`))
    })
    const asModule = await loaded({
      ...counts,
      ...moduleJudge("counts", `import * as fs from "node:fs"
const count = (file: string): number => fs.readFileSync(file, "utf8").split("\\n").filter((line) => line !== "").length
export default ({ context }: { context: any }) => [
  ...context.cells.map((cell: any) => {
    const n = count(cell.records.counts)
    return { verification: "counts", cell: cell.id, answer: n === cell.claims.counts.count ? "pass" : "fail", message: n + " record(s)" }
  }),
  { verification: "counts", answer: "pass", message: "unclaimed " + count(context.judging.unclaimed) + ", ambiguous " + count(context.judging.ambiguous) }
]
`)
    })
    const claimed = (cell: Cell.Cell): Cell.Cell => ({ ...cell, claims: { counts: { count: 2 } } })
    const judged = await Promise.all([asExecutable, asModule].map(async ({ root, rig }) => {
      const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"], claimed))
      return verdictsOf(byLabel(collected, "judge counts of counts"))
    }))
    expect(judged[0]).toEqual({
      cells: {
        c1: [{ name: "counts", source: "counts", answer: "pass", message: "2 record(s)" }],
        c2: [{ name: "counts", source: "counts", answer: "fail", message: "1 record(s)" }]
      },
      scope: [{ name: "counts", source: "counts", answer: "pass", message: "unclaimed 1, ambiguous 0" }]
    })
    expect(judged[1]).toEqual(judged[0])
  })

  it("lets a collector read the keys and records one it runs after added, whatever their names' order", async () => {
    const { root, rig } = await loaded({
      ...collector(
        "keyer",
        node(`for (const cell of ctx.cells) {
  console.log(JSON.stringify({ key: "trunk", cell: cell.id, values: ["t-" + cell.id] }))
  console.log(JSON.stringify({ record: { of: cell.id }, cell: cell.id }))
}`)
      ),
      ...collector("a-reader", node(`fs.copyFileSync(process.env.SIPRIG_CONTEXT, "../../reader-context.json")`), `after = ["keyer"]`)
    })
    const cells = cellsOf(root, ["c1", "c2"])
    const { collected } = await collectBatch(rig, root, cells)
    const read = JSON.parse(fs.readFileSync(path.join(root, "reader-context.json"), "utf8"))
    expect(collected.map((one) => one.label)).toEqual(["keyer", "a-reader"])
    expect(read.cells.map((cell: Cell.Cell) => [cell.keys.trunk, linesOf(cell.records.keyer!)])).toEqual([
      [["t-c1"], [{ of: "c1" }]],
      [["t-c2"], [{ of: "c2" }]]
    ])
    expect(JSON.parse(fs.readFileSync(path.join(root, "cells", "c1", "keys.json"), "utf8"))).toEqual({ trunk: ["t-c1"] })
  })
})

describe("a scope's exports", () => {
  it("are judged where no collector of their name runs, a line naming a cell by its directory's name read as that cell", async () => {
    const { root, rig } = await loaded(executableJudge("exp", node(`
fs.copyFileSync(process.env.SIPRIG_CONTEXT, "../../exp-context.json")
for (const cell of ctx.cells) console.log(JSON.stringify({ verification: "exp", cell: cell.id, answer: "pass" }))
`)))
    const cells = cellsOf(root, ["c1", "c2"], undefined, { c1: "c1~2" })
    writeExport(path.join(root, "cells", "c1~2"), "exp", [
      { record: { n: 1 }, cell: "c1~2" },
      { key: "ref", cell: "c1~2", values: ["r1"] },
      { record: { n: 3 }, cell: "c2" }
    ])
    writeExport(path.join(root, "batch"), "exp", [{ record: { n: 2 }, key: "c2@test" }])
    const { collected } = await collectBatch(rig, root, cells, true)
    const context = JSON.parse(fs.readFileSync(path.join(root, "exp-context.json"), "utf8"))
    expect(context.cells.map((cell: Cell.Cell) => [cell.id, cell.keys.ref ?? [], linesOf(cell.records.exp!)])).toEqual([
      ["c1", ["r1"], [{ n: 1 }]],
      ["c2", [], [{ n: 2 }]]
    ])
    expect(linesOf(context.judging.unclaimed)).toEqual([{ record: { n: 3 }, cell: "c2" }])
    expect(verdictsOf(byLabel(collected, "judge exp of exp")).cells).toEqual({
      c1: [{ name: "exp", source: "exp", answer: "pass" }],
      c2: [{ name: "exp", source: "exp", answer: "pass" }]
    })
  })

  it("are judged, where their collector is not wired, by the judge it names, answering what it answers, joined on its key", async () => {
    const { root, rig } = await loaded({
      ...collector("records", node(``), `needs = ["records-store"]\njudge = "differ"\nanswers = ["records-match", "records-ref"]\n[join]\nkey = "ref"\n`),
      "parts/records-store.schema.json": `{"type": "object"}`,
      ...executableJudge("differ", node(`
for (const cell of ctx.cells) console.log(JSON.stringify({ verification: "records-match", cell: cell.id, answer: fs.readFileSync(cell.records.records, "utf8") === "" ? "fail" : "pass" }))
console.log(JSON.stringify({ verification: "records-match", answer: "pass" }))
`))
    })
    const cells = cellsOf(root, ["c1", "c2"], (cell) => ({ ...cell, keys: { ...cell.keys, ref: [`r-${cell.id}`] } }))
    writeExport(path.join(root, "batch"), "records", [{ record: { n: 1 }, key: "r-c1" }])
    const { collected } = await collectBatch(rig, root, cells, true)
    expect([onCell(collected, "c1"), onCell(collected, "c2")]).toEqual([
      [
        { name: "records-ref", source: "differ", answer: "not-checkable", message: "nobody answered records-ref for the cell c1" },
        { name: "records-match", source: "differ", answer: "pass" }
      ],
      [
        { name: "records-ref", source: "differ", answer: "not-checkable", message: "nobody answered records-ref for the cell c2" },
        { name: "records-match", source: "differ", answer: "fail" }
      ]
    ])
  })

  /** A judge `exp` of exports alone, copying the records it reads and passing every cell. */
  const passingExp = executableJudge("exp", node(`
fs.writeFileSync("../../exp-read.json", JSON.stringify(ctx.cells.map((cell) => [cell.id, fs.readFileSync(cell.records.exp, "utf8")])))
for (const cell of ctx.cells) console.log(JSON.stringify({ verification: "exp", cell: cell.id, answer: "pass" }))
console.log(JSON.stringify({ verification: "exp", answer: "pass" }))
`))

  it("skip a cell's unreadable line, its other records joined, the name not checkable on that cell and the scope, the line said", async () => {
    const { root, rig } = await loaded(passingExp)
    const cells = cellsOf(root, ["c1", "c2"])
    writeExport(path.join(root, "cells", "c1"), "exp", [{ record: { n: 1 }, cell: "c1" }], [`{"record": {"n": 2}, "key": 5}`])
    writeExport(path.join(root, "cells", "c2"), "exp", [{ record: { n: 3 }, cell: "c2" }])
    const { collected, said } = await collectBatch(rig, root, cells, true)
    expect(JSON.parse(fs.readFileSync(path.join(root, "exp-read.json"), "utf8"))).toEqual([["c1", `{"n":1}\n`], ["c2", `{"n":3}\n`]])
    const judged = verdictsOf(byLabel(collected, "judge exp of exp"))
    const unread = [
      expect.objectContaining({ name: "exp", source: "exp", answer: "not-checkable", message: expect.stringContaining("exports/exp.jsonl line 2") })
    ]
    expect(judged).toEqual({ cells: { c1: unread, c2: [{ name: "exp", source: "exp", answer: "pass" }] }, scope: unread })
    expect(said).toEqual([expect.stringMatching(/^failed scope: exports: .*c1\/exports\/exp\.jsonl line 2: /)])
  })

  it.each([
    ["a key naming a directory no cell bears", { key: "ref", cell: "elsewhere", values: ["r1"] }, "names the cell directory elsewhere"],
    ["a verification line", { verification: "exp", cell: "c1", answer: "pass" }, "an export holds records and keys only"]
  ])("skip %s, the name not checkable on the cell whose export it is", async (_, line, why) => {
    const { root, rig } = await loaded(passingExp)
    writeExport(path.join(root, "cells", "c1"), "exp", [{ record: { n: 1 }, cell: "c1" }, line])
    writeExport(path.join(root, "cells", "c2"), "exp", [{ record: { n: 2 }, cell: "c2" }])
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]), true)
    expect([onCell(collected, "c1"), onCell(collected, "c2")]).toEqual([
      [expect.objectContaining({ name: "exp", source: "exp", answer: "not-checkable", message: expect.stringContaining(why) }), inputUnread(why)],
      [{ name: "exp", source: "exp", answer: "pass" }]
    ])
  })

  it("leave a judged name not checkable on a cell whose export of another name lost a line", async () => {
    const { root, rig } = await loaded(passingExp)
    writeExport(path.join(root, "cells", "c1"), "exp", [{ record: { n: 1 }, cell: "c1" }])
    writeExport(path.join(root, "cells", "c1"), "aux", [], [`{"key": 5, "cell": "c1", "values": ["r1"]}`])
    writeExport(path.join(root, "cells", "c2"), "exp", [{ record: { n: 2 }, cell: "c2" }])
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]), true)
    expect([onCell(collected, "c1"), onCell(collected, "c2")]).toEqual([
      [
        expect.objectContaining({ name: "exp", source: "exp", answer: "not-checkable", message: expect.stringContaining("exports/aux.jsonl line 1") }),
        inputUnread("exports/aux.jsonl line 1")
      ],
      [{ name: "exp", source: "exp", answer: "pass" }]
    ])
  })

  it("read past a cell whose exports are no directory, the name not checkable on that cell, the others judged", async () => {
    const { root, rig } = await loaded(passingExp)
    fs.mkdirSync(path.join(root, "cells", "c1"), { recursive: true })
    fs.writeFileSync(path.join(root, "cells", "c1", "exports"), "")
    writeExport(path.join(root, "cells", "c2"), "exp", [{ record: { n: 2 }, cell: "c2" }])
    const { collected, said } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]), true)
    expect([onCell(collected, "c1"), onCell(collected, "c2")]).toEqual([
      [
        expect.objectContaining({ name: "exp", source: "exp", answer: "not-checkable", message: expect.stringContaining("its exports are skipped") }),
        inputUnread("its exports are skipped")
      ],
      [{ name: "exp", source: "exp", answer: "pass" }]
    ])
    expect(said).toEqual([expect.stringMatching(/^failed scope: exports: .*c1\/exports: ENOTDIR/)])
  })

  it("keep a judge's fail on a cell whose export lost a line, not checkable beside it", async () => {
    const { root, rig } = await loaded(executableJudge("exp", node(`
for (const cell of ctx.cells) console.log(JSON.stringify({ verification: "exp", cell: cell.id, answer: "fail" }))
`)))
    writeExport(path.join(root, "cells", "c1"), "exp", [{ record: { n: 1 }, cell: "c1" }], ["not json"])
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1"]), true)
    expect(onCell(collected, "c1")).toEqual([
      { name: "exp", source: "exp", answer: "fail" },
      expect.objectContaining({ name: "exp", source: "exp", answer: "not-checkable", message: expect.stringContaining("line 2: not JSON") }),
      inputUnread("line 2: not JSON")
    ])
  })

  it("answer the name not checkable on a cell the base could not describe, and on the scope", async () => {
    const { root, rig } = await loaded(passingExp)
    writeExport(path.join(root, "cells", "c1"), "exp", [{ record: { n: 1 }, cell: "c1" }])
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1"]), true, [{ id: "c2", lane: LANE.id, why: "no cell.json" }])
    expect([onCell(collected, "c1"), onCell(collected, "c2"), onScope(collected)]).toEqual([
      [{ name: "exp", source: "exp", answer: "pass" }],
      [{ name: "exp", source: "exp", answer: "not-checkable", message: "the cell c2 was not described: no cell.json" }, inputUnread("the cell c2 was not described")],
      [
        expect.objectContaining({ name: "exp", source: "exp", answer: "not-checkable", message: expect.stringContaining("the cell c2 was not described") }),
        inputUnread("the cell c2 was not described")
      ]
    ])
  })

  it("answer the name not checkable on a cell the base could not describe where no export of the name was read", async () => {
    const { root, rig } = await loaded(passingExp)
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1"]), true, [{ id: "c2", lane: LANE.id, why: "no cell.json" }])
    expect([onCell(collected, "c1"), onCell(collected, "c2")]).toEqual([
      [],
      [{ name: "exp", source: "exp", answer: "not-checkable", message: "the cell c2 was not described: no cell.json" }, inputUnread("the cell c2 was not described")]
    ])
  })

  it("judge a name whose only export is unreadable, not checkable on every cell and the scope", async () => {
    const { root, rig } = await loaded(passingExp)
    const cells = cellsOf(root, ["c1", "c2"])
    writeExport(path.join(root, "batch"), "exp", [], ["not json"])
    const { collected } = await collectBatch(rig, root, cells, true)
    const judged = verdictsOf(byLabel(collected, "judge exp of exp"))
    const unread = [expect.objectContaining({ name: "exp", source: "exp", answer: "not-checkable", message: expect.stringContaining("line 1: not JSON") })]
    expect(judged).toEqual({ cells: { c1: unread, c2: unread }, scope: unread })
  })
})

describe("a scope's input not handed whole", () => {
  /** A collector `checks` answering `checks` pass on every cell and the scope. */
  const passingChecks = collector(
    "checks",
    node(`for (const cell of ctx.cells) console.log(JSON.stringify({ verification: "checks", cell: cell.id, answer: "pass" }))
console.log(JSON.stringify({ verification: "checks", answer: "pass" }))`),
    `answers = ["checks"]`
  )

  const notCheckable = (why: string, source = "checks", name = source) =>
    [expect.objectContaining({ name, source, answer: "not-checkable", message: expect.stringContaining(why) })]

  it("leaves a collector's pass not checkable on a cell whose export lost a line, and on the scope", async () => {
    const { root, rig } = await loaded(passingChecks)
    writeExport(path.join(root, "cells", "c1"), "aux", [], ["not json"])
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    expect(verdictsOf(byLabel(collected, "checks"))).toEqual({
      cells: { c1: notCheckable("exports/aux.jsonl line 1"), c2: [{ name: "checks", source: "checks", answer: "pass" }] },
      scope: notCheckable("exports/aux.jsonl line 1")
    })
  })

  it.each([
    ["an export adds", passingChecks, (root: string) => writeExport(path.join(root, "cells", "c1"), "aux", [{ key: "ref", cell: "c1", values: ["r1"] }]), "exported keys: "],
    [
      "an earlier collector adds",
      {
        ...collector("adder", node(`console.log(JSON.stringify({ key: "ref", cell: "c1", values: ["r1"] }))`)),
        ...collector("checks", passingChecks["collectors/checks/collect"]!, `answers = ["checks"]\nafter = ["adder"]`)
      },
      () => undefined,
      "adder: its keys: "
    ]
  ])("leave every later pass not checkable where the keys %s could not be written", async (_, files, lay, why) => {
    const { root, rig } = await loaded(files)
    lay(root)
    fs.mkdirSync(path.join(root, "cells", "c1", "keys.json"), { recursive: true })
    const { collected, said } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    const unwritten = notCheckable(why)
    expect(verdictsOf(byLabel(collected, "checks"))).toEqual({ cells: { c1: unwritten, c2: unwritten }, scope: unwritten })
    expect(said.filter((line) => line.startsWith("failed"))).toEqual([expect.stringContaining(`failed scope: ${why}EISDIR`)])
  })

  it("leaves every later pass not checkable where the keys file a cell holds is unreadable, the file left as it was", async () => {
    const { root, rig } = await loaded(passingChecks)
    writeExport(path.join(root, "cells", "c1"), "aux", [{ key: "ref", cell: "c1", values: ["r1"] }])
    const keys = path.join(root, "cells", "c1", "keys.json")
    fs.writeFileSync(keys, `{"trunk": ["t1"]`)
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    const unwritten = notCheckable("exported keys: ")
    expect(verdictsOf(byLabel(collected, "checks"))).toEqual({ cells: { c1: unwritten, c2: unwritten }, scope: unwritten })
    expect(fs.readFileSync(keys, "utf8")).toBe(`{"trunk": ["t1"]`)
  })

  it.each([
    ["the scope's export", "batch", ["c1", "c2"]],
    ["a cell's export", "cells/c1", ["c1"]]
  ])("writes the base's own not checkable line where %s lost a line, on each cell it concerns and the scope, though no entry answered there", async (_, dir, marked) => {
    const { root, rig } = await loaded(collector("calls", node(`console.log(JSON.stringify({ record: { n: 1 }, key: "c1@test" }))`), `[join]\nkey = "call_id"`))
    writeExport(path.join(root, dir), "aux", [], ["not json"])
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    expect(linesOf(path.join(root, "cells", "c1", "joined", "calls.jsonl"))).toEqual([{ n: 1 }])
    const unread = [inputUnread("exports/aux.jsonl line 1")]
    expect([onCell(collected, "c1"), onCell(collected, "c2"), onScope(collected)]).toEqual([
      unread,
      marked.includes("c2") ? unread : [],
      unread
    ])
  })

  /** A collector `calls` keyed by Call-ID whose judge `trusting` passes every cell and the scope without reading a record. */
  const trustedCalls = {
    ...collector("calls", node(`console.log(JSON.stringify({ record: { n: 1 }, key: "c2@test" }))`), `judge = "trusting"\n[join]\nkey = "call_id"`),
    ...executableJudge("trusting", node(`
for (const cell of ctx.cells) console.log(JSON.stringify({ verification: "calls", cell: cell.id, answer: "pass" }))
console.log(JSON.stringify({ verification: "calls", answer: "pass" }))
`))
  }

  it.each([
    ["a cell's joined records", "cells/c1/joined", "the records of calls not written"],
    ["the scope's unclaimed and ambiguous records", "batch/joined", "the unclaimed and ambiguous records of calls not kept"]
  ])("runs no judge whose %s could not be written, its verifications not checkable", async (_, blocked, why) => {
    const { root, rig } = await loaded(trustedCalls)
    fs.mkdirSync(path.dirname(path.join(root, blocked)), { recursive: true })
    fs.writeFileSync(path.join(root, blocked), "")
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    const unwritten = notCheckable(`judge trusting not run: ${why}`, "trusting", "calls")
    expect(verdictsOf(byLabel(collected, "judge trusting of calls"))).toEqual({ cells: { c1: unwritten, c2: unwritten }, scope: unwritten })
    expect(fs.existsSync(path.join(root, "batch", "judges", "calls"))).toBe(false)
  })
})

describe("a verification nobody could answer", () => {
  it("is not checkable on every cell and the scope where the judge fails, quoting why", async () => {
    const { root, rig } = await loaded({
      ...collector("broken", node(`console.log(JSON.stringify({ record: { n: 1 }, key: "c1@test" }))`), `judge = "crashes"\nanswers = ["sums"]\n[join]\nkey = "call_id"`),
      ...executableJudge("crashes", node(`console.error("the sums do not add up"); process.exit(4)`))
    })
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    const judged = verdictsOf(byLabel(collected, "judge crashes of broken"))
    expect(judged.scope).toEqual([
      expect.objectContaining({ name: "sums", source: "crashes", answer: "not-checkable", message: expect.stringContaining("judge exited 4") })
    ])
    expect(Object.keys(judged.cells)).toEqual(["c1", "c2"])
    expect(judged.cells.c2).toEqual(judged.scope)
  })

  /** A collector `sums` keyed by Call-ID whose judge `judge` answers `sums`. */
  const judgedSums = (judge: string) =>
    collector("sums", node(`console.log(JSON.stringify({ record: { n: 1 }, key: "c1@test" }))`), `judge = "${judge}"\nanswers = ["sums"]\n[join]\nkey = "call_id"`)

  it.each([
    ["returns no iterable of lines", `export default () => undefined`, "", "returned undefined, no iterable of lines"],
    [
      "returns an Effect that dies",
      `import * as Effect from "${EFFECT_URL}"\nexport default () => Effect.sync(() => { throw new Error("the sums died") })`,
      "",
      "died: the sums died"
    ],
    ["returns a rejected Promise", `export default async () => { throw new Error("the sums rejected") }`, "", "rejected: the sums rejected"],
    ["answers a verification it may not answer", `export default () => [{ verification: "other", answer: "pass" }]`, "", "answers other, which it may not answer"],
    ["names a cell outside its scope", `export default () => [{ verification: "sums", cell: "c9", answer: "pass" }]`, "", "names the cell c9"],
    ["outlives its timeout", `export default () => new Promise(() => {})`, "[timeouts]\njudge_s = 0.05", "timed out after 50 ms"]
  ])("is not checkable on every cell and the scope where a module judge %s", async (_, text, extra, why) => {
    const { root, rig } = await loaded({ ...judgedSums("adds"), ...moduleJudge("adds", text, extra) })
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    const judged = verdictsOf(byLabel(collected, "judge adds of sums"))
    const failed = [expect.objectContaining({ name: "sums", source: "adds", answer: "not-checkable", message: expect.stringContaining(why) })]
    expect(judged).toEqual({ cells: { c1: failed, c2: failed }, scope: failed })
  })

  it("reads an executable judge's lines on its stdout and in SIPRIG_OUT/lines.jsonl", async () => {
    const { root, rig } = await loaded({
      ...judgedSums("adds"),
      ...executableJudge("adds", node(`
console.log(JSON.stringify({ verification: "sums", cell: "c1", answer: "pass" }))
fs.writeFileSync(require("node:path").join(process.env.SIPRIG_OUT, "lines.jsonl"), JSON.stringify({ verification: "sums", cell: "c2", answer: "fail" }) + "\\n")
`))
    })
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    expect(verdictsOf(byLabel(collected, "judge adds of sums")).cells).toEqual({
      c1: [{ name: "sums", source: "adds", answer: "pass" }],
      c2: [{ name: "sums", source: "adds", answer: "fail" }]
    })
  })

  it("is not checkable, under the judge's name, for the cell a collector's judge left unanswered, the collector naming no answers but its judge", async () => {
    const { root, rig } = await loaded({
      ...collector("half", node(`console.log(JSON.stringify({ record: { n: 1 }, key: "c1@test" }))`), `judge = "halves"\n[join]\nkey = "call_id"`),
      ...executableJudge("halves", node(`console.log(JSON.stringify({ verification: "half", cell: "c1", answer: "pass" }))`))
    })
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    expect([onCell(collected, "c1"), onCell(collected, "c2")]).toEqual([
      [{ name: "half", source: "halves", answer: "pass" }],
      [{ name: "half", source: "halves", answer: "not-checkable", message: "nobody answered half for the cell c2" }]
    ])
  })

  it("is the judge's own informative line on a cell, nothing filled beside it: the cell passes on its other lines", async () => {
    const { root, rig } = await loaded({
      ...collector("half", node(`console.log(JSON.stringify({ record: { n: 1 }, key: "c1@test" }))`), `judge = "halves"\n[join]\nkey = "call_id"`),
      ...executableJudge("halves", node(`
console.log(JSON.stringify({ verification: "half", cell: "c1", answer: "pass" }))
console.log(JSON.stringify({ verification: "half", cell: "c2", answer: "pass", message: "c2 holds nothing to judge", informative: true }))
`))
    })
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    const c2 = onCell(collected, "c2")
    expect(c2).toEqual([{ name: "half", source: "halves", answer: "pass", message: "c2 holds nothing to judge", informative: true }])
    expect(Verification.finalStatusOf([{ name: "replay", source: "replay", answer: "pass" }, ...c2])).toBe("pass")
  })

  it("is not checkable for the cell a judge of exports alone left unanswered", async () => {
    const { root, rig } = await loaded(executableJudge("exp", node(`console.log(JSON.stringify({ verification: "exp", cell: "c1", answer: "pass" }))`)))
    writeExport(path.join(root, "batch"), "exp", [{ record: { n: 1 }, key: "c1@test" }, { record: { n: 2 }, key: "c2@test" }])
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]), true)
    expect([onCell(collected, "c1"), onCell(collected, "c2")]).toEqual([
      [{ name: "exp", source: "exp", answer: "pass" }],
      [{ name: "exp", source: "exp", answer: "not-checkable", message: "nobody answered exp for the cell c2" }]
    ])
  })

  /**
   * A collector `sums` keyed by Call-ID, its steps `steps`, of `scope` (its
   * manifest's `extra` lines with it), whose executable judge `strict` fails
   * every cell holding no record of it.
   */
  const strictSums = (steps: Readonly<Record<string, Entry | string>>, scope = "batch", extra = ""): Record<string, Entry> => ({
    "collectors/sums/collector.toml": `schema = "collector.v1"\nname = "sums"\nscope = "${scope}"\njudge = "strict"\nanswers = ["sums"]\n${extra}` +
      `[join]\nkey = "call_id"\n[run]\n${Object.keys(steps).map((step) => `${step} = "./${step}"\n`).join("")}`,
    ...Object.fromEntries(
      Object.entries(steps).map(([step, body]) => [`collectors/sums/${step}`, typeof body === "string" ? { text: body, executable: true } : body])
    ),
    ...executableJudge(
      "strict",
      node(`
const held = (file) => file !== undefined && fs.readFileSync(file, "utf8").trim() !== ""
for (const cell of ctx.cells) console.log(JSON.stringify({ verification: "sums", cell: cell.id, answer: held(cell.records.sums) ? "pass" : "fail" }))
`)
    )
  })

  it.each([
    ["its collect exits non-zero", { collect: `#!/bin/sh\necho "the store is gone" >&2\nexit 3\n` }, "collect exited 3: the store is gone"],
    ["its open exits non-zero", { open: `#!/bin/sh\necho "no route to the store" >&2\nexit 2\n`, collect: `#!/bin/sh\n` }, "open exited 2: no route to the store"]
  ])("is not checkable, its judge not run, on every cell a collector did not collect because %s", async (_, steps, why) => {
    const { root, rig } = await loaded(strictSums(steps))
    const { collected } = await collectBatch(rig, root, cellsOf(root, ["c1", "c2"]))
    const failed = [expect.objectContaining({ name: "sums", source: "strict", answer: "not-checkable", message: expect.stringContaining(why) })]
    expect([onCell(collected, "c1"), onCell(collected, "c2"), onScope(collected)]).toEqual([failed, failed, failed])
    for (const id of ["c1", "c2"]) expect(Verification.finalStatusOf(onCell(collected, id))).toBe("unverified")
    expect(collected.map((one) => one.label)).toEqual(["sums"])
  })

  it("is not checkable on the cells of the one instance whose collect failed, the cells of the instance that collected judged", async () => {
    const collect = node(`
if (ctx.lane.name === "lane-a") { console.error("the store is gone"); process.exit(3) }
for (const cell of ctx.cells) console.log(JSON.stringify({ record: { n: 1 }, key: cell.keys.call_id[0] }))
`)
    const { root, rig } = await loaded(strictSums({ collect }, "job", `needs = ["records-store"]\n`))
    const lanes = [laneOf("lane-a", { "records-store": "targets/a.toml" }), laneOf("lane-b", { "records-store": "targets/b.toml" })]
    const cells = [
      ...cellsOf(root, ["a1", "a2"]),
      ...cellsOf(root, ["b1", "b2"], (cell) => ({ ...cell, lane: "lane-b" }))
    ]
    const collected = await collectJob(rig, root, lanes, cells)
    const why = "collect exited 3: the store is gone"
    const notCollected = [expect.objectContaining({ name: "sums", source: "strict", answer: "not-checkable", message: expect.stringContaining(why) })]
    expect([onCell(collected, "a1"), onCell(collected, "a2")]).toEqual([notCollected, notCollected])
    for (const id of ["a1", "a2"]) expect(Verification.finalStatusOf(onCell(collected, id))).toBe("unverified")
    expect([onCell(collected, "b1"), onCell(collected, "b2")]).toEqual([
      [{ name: "sums", source: "strict", answer: "pass" }],
      [{ name: "sums", source: "strict", answer: "pass" }]
    ])
  })

  it("is not checkable for each cell a collector's manifest names it on and nobody answered it, the cell's own lines counted", async () => {
    const { root, rig } = await loaded(collector("silent", node(""), `answers = ["quiet"]`))
    const cells = cellsOf(root, ["c1", "c2"], (cell) =>
      cell.id === "c2" ? { ...cell, verifications: [{ name: "quiet", source: "interpreter", answer: "pass" }] } : cell
    )
    const { collected } = await collectBatch(rig, root, cells)
    expect(verdictsOf(byLabel(collected, "silent")).cells).toEqual({
      c1: [{ name: "quiet", source: "silent", answer: "not-checkable", message: "nobody answered quiet for the cell c1" }],
      c2: []
    })
  })
})
