/**
 * A scope's collectors, run as real executables: opened before the scope,
 * collected over its cells, closed after it whatever ended it; `SIPRIG_STATE`
 * kept from `open` to `close`; the lines of `collect` read from its stdout and
 * `SIPRIG_OUT`, its attachments copied and listed in their holder's
 * directory; a failed, timed-out or unreadable `open` or `collect` answering
 * `not checkable` on every cell, as on a cell the base could not describe; a
 * `close` failing twice marking the lab left changed; locks held from `open`
 * to `close`; an `on-failure` collector collecting the failed cells alone;
 * an on-demand run over the cells named.
 */
import { Attachment, Verification } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import type * as Cell from "../src/cell.js"
import * as CollectorOnDemand from "../src/collector-on-demand.js"
import * as CollectorPlan from "../src/collector-plan.js"
import * as CollectorRun from "../src/collector-run.js"
import * as RigPath from "../src/rig-path.js"
import { alive, cellOf, goneWithin, memoryLocker, SAMPLES, traceOf, tracedWithin } from "./collectors.js"
import { laneOf } from "./operations.js"
import { run, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const LANE = laneOf("lane-a", { "records-store": "targets/records-store/main.toml" })

/** A rig path holding every sample collector, loaded; its root. */
const samples = async () => {
  const root = tree(SAMPLES)
  made.push(root)
  const loaded = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(loaded)) throw new Error(`the samples do not load: ${String(loaded.failure)}`)
  return { root, rig: loaded.success }
}

/** The batch instance of collector `name` under `root`, on {@link LANE}. */
const instanceOf = (rig: RigPath.RigPath, root: string, name: string): CollectorRun.Instance => {
  const collector = rig.collectors.get(name)!
  return {
    collector,
    id: name,
    label: name,
    scope: "batch",
    lane: LANE,
    lanes: undefined,
    params: CollectorPlan.paramsOf(collector.manifest, LANE),
    dir: path.join(root, "batch", "collectors", name),
    holder: path.join(root, "batch")
  }
}

/** Two cells of {@link LANE}, their directories under `root`. */
const cellsUnder = (root: string): ReadonlyArray<Cell.Cell> =>
  ["c1", "c2"].map((id) => cellOf(id, LANE.id, path.join(root, "cells", id)))

/** A sink collecting each event, and the phases it saw as `<phase> <collector>`. */
const collecting = () => {
  const events: Array<CollectorRun.Event> = []
  const sink: CollectorRun.Sink = (event) => Effect.sync(() => void events.push(event))
  return { events, sink, phases: () => events.map((event) => `${event.phase} ${event.collector}`) }
}

/** The scope `body` run between opening `instances` and collecting them over `cells`; what they collected and the lab's state. */
const scope = <E>(
  instances: ReadonlyArray<CollectorRun.Instance>,
  cells: ReadonlyArray<Cell.Cell>,
  options: CollectorRun.Options,
  body: Effect.Effect<void, E> = Effect.void
) =>
  Effect.gen(function*() {
    let opened: CollectorRun.Opened | undefined
    const collected = yield* Effect.exit(Effect.scoped(Effect.gen(function*() {
      opened = yield* CollectorRun.opened(instances, options)
      yield* body
      return yield* opened.collect({ cells, unread: [], failed: [] })
    })))
    return { collected, leftChanged: opened?.leftChanged() ?? [] }
  })

const runScope = <E>(
  instances: ReadonlyArray<CollectorRun.Instance>,
  cells: ReadonlyArray<Cell.Cell>,
  options: CollectorRun.Options,
  body?: Effect.Effect<void, E>
) => Effect.runPromise(scope(instances, cells, options, body))

/** The collected answers of the scope, which must have collected. */
const valueOf = <A, E>(exit: Exit.Exit<A, E>): A => {
  if (!Exit.isSuccess(exit)) throw new Error(`the scope did not collect: ${String(exit.cause)}`)
  return exit.value
}

describe("a scope's collectors", () => {
  it("open before the scope, collect over its cells and close after it, sharing SIPRIG_STATE, each step an event", async () => {
    const { root, rig } = await samples()
    const { sink, phases } = collecting()
    const cells = cellsUnder(root)
    const { collected } = await runScope([instanceOf(rig, root, "tidy")], cells, { sink, locker: memoryLocker() })
    valueOf(collected)
    expect(traceOf(root)).toEqual(["open tidy open", "collect tidy token collect", "close tidy token close"])
    expect(fs.readFileSync(path.join(root, "state-seen"), "utf8")).toBe("")
    expect(phases()).toEqual(["open tidy", "opened tidy", "collect tidy", "collected tidy", "close tidy", "closed tidy"])
  })

  it("gives open a context with no cell and collect one with the scope's cells, its window and the lane's parameters", async () => {
    const { root, rig } = await samples()
    const cells = cellsUnder(root)
    await runScope([instanceOf(rig, root, "tidy")], cells, { sink: () => Effect.void, locker: memoryLocker() })
    const opened = JSON.parse(fs.readFileSync(path.join(root, "open-context.json"), "utf8"))
    const collected = JSON.parse(fs.readFileSync(path.join(root, "collect-context.json"), "utf8"))
    expect(opened).toMatchObject({ schema: "context.v1", scope: "batch", cells: [], params: { poll_s: 1 }, lane: { name: "lane-a" } })
    expect(opened.window.end).toBeUndefined()
    expect(collected.cells.map((cell: Cell.Cell) => cell.id)).toEqual(["c1", "c2"])
    expect(collected.window.start).toEqual(opened.window.start)
    expect(collected.window.end.wall_ms).toBeGreaterThanOrEqual(opened.window.start.wall_ms)
  })

  it("reads its lines from stdout and SIPRIG_OUT: each cell's verification, attachment and key, the scope's line, a record", async () => {
    const { root, rig } = await samples()
    const cells = cellsUnder(root)
    const { collected } = await runScope([instanceOf(rig, root, "tidy")], cells, { sink: () => Effect.void, locker: memoryLocker() })
    const [tidy] = valueOf(collected)
    const attachment = { name: "tidy", kind: "log", title: "its log", source: "tidy" }
    for (const cell of cells) {
      expect(tidy!.cells.get(cell.id)).toEqual({
        verifications: [{ name: "tidy", source: "tidy", answer: "pass" }],
        attachments: [{ ...attachment, file: `collected/tidy/${cell.id}.log` }]
      })
      const listed = Attachment.parse(fs.readFileSync(path.join(cell.dir, Attachment.FILE), "utf8"))
      expect(listed).toEqual([{ ...attachment, file: `collected/tidy/${cell.id}.log` }])
      expect(fs.readFileSync(path.join(cell.dir, listed[0]!.file), "utf8")).toBe(`log of ${cell.id}\n`)
    }
    expect(tidy!.scope.verifications).toEqual([{ name: "tidy", source: "tidy", answer: "fail", message: "the scope's own" }])
    expect(tidy!.keys).toEqual(cells.map((cell) => ({ key: "trunk", cell: cell.id, values: ["t1"] })))
    expect(tidy!.assigned).toEqual([{ record: { n: 1 }, cell: "c1" }])
    const out = path.join(root, "batch", "collectors", "tidy", "out")
    expect(fs.existsSync(out) ? fs.readdirSync(out) : []).toEqual([])
  })

  it("answers not checkable, saying why, on a cell of its lanes the base could not describe, and keeps its out/ where collect failed", async () => {
    const { root, rig } = await samples()
    const cells = cellsUnder(root)
    const unread: ReadonlyArray<CollectorRun.Unread> = [
      { id: "c3", lane: LANE.id, why: "its bundle states no wall-clock window" },
      { id: "c4", lane: "lane-b", why: "its bundle is gone" }
    ]
    const lanes = { ...instanceOf(rig, root, "tidy"), lanes: [LANE.id] }
    const failing = instanceOf(rig, root, "failing-collect")
    const collected = await Effect.runPromise(Effect.scoped(Effect.flatMap(
      CollectorRun.opened([lanes, failing], { sink: () => Effect.void, locker: memoryLocker() }),
      (opened) => opened.collect({ cells, unread, failed: [] })
    )))
    const [tidy, failed] = collected
    expect(tidy!.cells.get("c1")!.verifications).toEqual([{ name: "tidy", source: "tidy", answer: "pass" }])
    expect(tidy!.cells.get("c3")).toEqual({
      verifications: [{
        name: "tidy",
        source: "tidy",
        answer: "not-checkable",
        message: "the cell c3 was not described: its bundle states no wall-clock window"
      }],
      attachments: []
    })
    expect(tidy!.cells.has("c4")).toBe(false)
    expect(failed!.cells.get("c4")!.verifications.map((line) => [line.name, line.answer, line.message])).toEqual([
      ["records", "not-checkable", "the cell c4 was not described: its bundle is gone"],
      ["count", "not-checkable", "the cell c4 was not described: its bundle is gone"]
    ])
    expect(fs.existsSync(path.join(root, "batch", "collectors", "failing-collect", "out"))).toBe(true)
  })

  it("collects an on-failure collector over the cells failed by then alone: their own lines, or an earlier collector's", async () => {
    const { root, rig } = await samples()
    const [c1, c2, c3] = ["c1", "c2", "c3"].map((id) => cellOf(id, LANE.id, path.join(root, "cells", id)))
    const failedCall = { name: "call", source: "interpreter", answer: "fail" as const }
    const cells = [
      { ...c1!, verifications: [failedCall] },
      c2!,
      { ...c3!, verifications: [{ ...failedCall, waived: true }] }
    ]
    const onFailure = { ...instanceOf(rig, root, "on-failure"), onFailure: true }
    const { collected } = await runScope([instanceOf(rig, root, "fails-c2"), onFailure], cells, {
      sink: () => Effect.void,
      locker: memoryLocker()
    })
    const [, answered] = valueOf(collected)
    expect(traceOf(root)).toEqual(["open on-failure", "collect on-failure c1,c2", "close on-failure"])
    expect([...answered!.cells.keys()]).toEqual(["c1", "c2"])
  })

  it("collects an on-failure collector after every other of its scope, one ordered after it included", async () => {
    const { root, rig } = await samples()
    const onFailure = { ...instanceOf(rig, root, "on-failure"), onFailure: true }
    const { collected } = await runScope([onFailure, instanceOf(rig, root, "fails-c2")], cellsUnder(root), {
      sink: () => Effect.void,
      locker: memoryLocker()
    })
    const answers = valueOf(collected)
    const answered = answers.find((one) => one.label === "on-failure")
    expect(traceOf(root)).toEqual(["open on-failure", "collect on-failure c2", "close on-failure"])
    expect(answers.map((one) => one.label)).toEqual(["fails-c2", "on-failure"])
    expect([...answered!.cells.keys()]).toEqual(["c2"])
  })

  it("collects an on-failure collector over the cells its caller states failed or errored, though no line of theirs failed", async () => {
    const { root, rig } = await samples()
    const onFailure = { ...instanceOf(rig, root, "on-failure"), onFailure: true }
    const collected = await Effect.runPromise(Effect.scoped(Effect.flatMap(
      CollectorRun.opened([onFailure], { sink: () => Effect.void, locker: memoryLocker() }),
      (opened) => opened.collect({ cells: cellsUnder(root), unread: [], failed: ["c2"] })
    )))
    expect(traceOf(root)).toEqual(["open on-failure", "collect on-failure c2", "close on-failure"])
    expect([...collected[0]!.cells.keys()]).toEqual(["c2"])
  })

  it("opens and closes an on-failure collector but never collects it where no cell failed, which answers nothing", async () => {
    const { root, rig } = await samples()
    const { sink, phases } = collecting()
    const onFailure = { ...instanceOf(rig, root, "on-failure"), onFailure: true }
    const { collected } = await runScope([onFailure], cellsUnder(root), { sink, locker: memoryLocker() })
    const [answered] = valueOf(collected)
    expect(traceOf(root)).toEqual(["open on-failure", "close on-failure"])
    expect(phases()).toEqual(["open on-failure", "opened on-failure", "close on-failure", "closed on-failure"])
    expect(answered).toEqual({
      label: "on-failure",
      scope: { verifications: [], attachments: [] },
      cells: new Map(),
      keyed: [],
      assigned: [],
      keys: []
    })
  })

  it("runs an on-demand collector over the cells named, as a scope of its own, attaching to each cell", async () => {
    const { root, rig } = await samples()
    const planned = CollectorPlan.onDemandOf(rig, "on-demand", LANE, path.join(root, "asked", "collectors"), path.join(root, "asked"))
    if (!Result.isSuccess(planned)) throw new Error(planned.failure)
    const [c1] = cellsUnder(root)
    const ran = await Effect.runPromise(
      CollectorOnDemand.run([planned.success], { cells: [c1!], unread: [], failed: [] }, { sink: () => Effect.void, locker: memoryLocker() })
    )
    expect(traceOf(root)).toEqual(["collect on-demand c1", "window 0-1000"])
    expect(ran.leftChanged).toEqual([])
    const listed = Attachment.parse(fs.readFileSync(path.join(c1!.dir, Attachment.FILE), "utf8"))
    expect(listed).toEqual([{ name: "on-demand", kind: "log", title: "asked now", file: "collected/on-demand/c1.log", source: "on-demand" }])
    expect(fs.readFileSync(path.join(c1!.dir, listed[0]!.file), "utf8")).toBe("asked of c1\n")
  })

  it("states an on-demand run's window as the one its cells ran in, long before it was asked", async () => {
    const { root, rig } = await samples()
    const planned = CollectorPlan.onDemandOf(rig, "on-demand", LANE, path.join(root, "asked", "collectors"), path.join(root, "asked"))
    if (!Result.isSuccess(planned)) throw new Error(planned.failure)
    const [c1, c2] = cellsUnder(root)
    const at = (cell: typeof c1, from: number, to: number) => ({
      ...cell!,
      timeline: { start: { lane_ms: 0, wall_ms: from }, end: { lane_ms: to - from, wall_ms: to } }
    })
    await Effect.runPromise(CollectorOnDemand.run(
      [planned.success],
      { cells: [at(c2, 5_000, 7_000), at(c1, 1_000, 2_000)], unread: [], failed: [] },
      { sink: () => Effect.void, locker: memoryLocker() }
    ))
    expect(traceOf(root)).toEqual(["collect on-demand c2,c1", "window 1000-7000"])
  })

  it("answers not checkable, with what the collector said, for every verification it names on every cell when collect exits non-zero", async () => {
    const { root, rig } = await samples()
    const cells = cellsUnder(root)
    const { events, sink } = collecting()
    const { collected } = await runScope([instanceOf(rig, root, "failing-collect")], cells, { sink, locker: memoryLocker() })
    const [failed] = valueOf(collected)
    const log = path.join(root, "batch", "collectors", "failing-collect", "collect.log")
    const message = `collect exited 3: the records store is down, its log ${log}`
    const lines = ["records", "count"].map((name) => ({ name, source: "failing-collect", answer: "not-checkable", message }))
    for (const cell of cells) {
      expect(failed!.cells.get(cell.id)!.verifications).toEqual(lines)
      expect(Verification.finalStatusOf(failed!.cells.get(cell.id)!.verifications)).toBe("unverified")
    }
    expect(failed!.scope.verifications).toEqual(lines)
    expect(events.find((event) => event.phase === "failed")?.message).toBe(message)
    expect(traceOf(root)).toEqual(["collect failing-collect", "close failing-collect"])
  })

  it("does not collect a collector whose open failed, answers not checkable with the open's message, and still closes it", async () => {
    const { root, rig } = await samples()
    const cells = cellsUnder(root)
    const { collected } = await runScope([instanceOf(rig, root, "failing-open")], cells, { sink: () => Effect.void, locker: memoryLocker() })
    const [failed] = valueOf(collected)
    const log = path.join(root, "batch", "collectors", "failing-open", "open.log")
    expect(failed!.cells.get("c1")!.verifications).toEqual([
      { name: "failing-open", source: "failing-open", answer: "not-checkable", message: `open exited 2: no route to the store, its log ${log}` }
    ])
    expect(traceOf(root)).toEqual(["open failing-open", "close failing-open"])
  })

  it("kills a collect past its timeout with every process it started, and answers not checkable", async () => {
    const { root, rig } = await samples()
    const cells = cellsUnder(root)
    const { collected } = await runScope([instanceOf(rig, root, "hanging-collect")], cells, { sink: () => Effect.void, locker: memoryLocker() })
    const [hung] = valueOf(collected)
    expect(hung!.cells.get("c2")!.verifications[0]!.message).toMatch(/^collect timed out after 500 ms/)
    const sleeper = Number(fs.readFileSync(path.join(root, "sleeper"), "utf8"))
    expect(await goneWithin(sleeper)).toBe(true)
  })

  it("lets open leave a process running through the scope, which close ends", async () => {
    const { root, rig } = await samples()
    let duringScope: boolean | undefined
    const body = Effect.sync(() => {
      duringScope = alive(Number(fs.readFileSync(path.join(root, "sleeper"), "utf8")))
    })
    await runScope([instanceOf(rig, root, "leaves-process")], cellsUnder(root), { sink: () => Effect.void, locker: memoryLocker() }, body)
    expect(duringScope).toBe(true)
    const sleeper = Number(fs.readFileSync(path.join(root, "sleeper"), "utf8"))
    expect(await goneWithin(sleeper)).toBe(true)
  })

  it("closes, in reverse order, when the scope fails after open, and does not collect", async () => {
    const { root, rig } = await samples()
    const { collected } = await runScope(
      ["tidy", "leaves-process"].map((name) => instanceOf(rig, root, name)),
      cellsUnder(root),
      { sink: () => Effect.void, locker: memoryLocker() },
      Effect.fail("the batch broke")
    )
    expect(collected).toEqual(Exit.fail("the batch broke"))
    expect(traceOf(root)).toEqual(["open tidy open", "open leaves-process", "close leaves-process", "close tidy token close"])
  })

  it("closes when the scope is interrupted after open, and when an open under way is interrupted", async () => {
    const { root, rig } = await samples()
    const fiber = Effect.runFork(scope(
      ["tidy", "slow-open"].map((name) => instanceOf(rig, root, name)),
      cellsUnder(root),
      { sink: () => Effect.void, locker: memoryLocker() },
      Effect.never
    ))
    expect(await tracedWithin(root, "open slow-open")).toBe(true)
    const interrupted = Date.now()
    await Effect.runPromise(Fiber.interrupt(fiber))
    expect(Date.now() - interrupted).toBeLessThan(4000)
    expect(traceOf(root)).toEqual(["open tidy open", "open slow-open", "close slow-open", "close tidy token close"])
  })

  it("marks the lab left changed when close fails twice, and closes the others still", async () => {
    const { root, rig } = await samples()
    const { events, sink, phases } = collecting()
    const { leftChanged } = await runScope(
      ["tidy", "failing-close", "flaky-close"].map((name) => instanceOf(rig, root, name)),
      cellsUnder(root),
      { sink, locker: memoryLocker() }
    )
    expect(leftChanged).toEqual(["failing-close"])
    expect(phases().filter((phase) => /^(close|closed|failed|lab-left-changed) /.test(phase))).toEqual([
      "close flaky-close",
      "failed flaky-close",
      "close flaky-close",
      "closed flaky-close",
      "close failing-close",
      "failed failing-close",
      "close failing-close",
      "failed failing-close",
      "lab-left-changed failing-close",
      "close tidy",
      "closed tidy"
    ])
    expect(events.find((event) => event.phase === "lab-left-changed")?.message).toMatch(/^2 close attempts failed, the last: close exited 1/)
  })

  it.each([
    ["not-json", "collect: its stdout line 1: not JSON"],
    ["stranger", "collect: its stdout line 1: answers someone-else, which its manifest does not name"],
    ["wrong-cell", "collect: its stdout line 1: names the cell c9, which is not one of its scope"],
    ["lost-file", "collect: its stdout line 1: attaches gone.log, which is no file under SIPRIG_OUT"]
  ])("answers not checkable where %s answers a line that is none of its", async (name, message) => {
    const { root, rig } = await samples()
    const { collected } = await runScope([instanceOf(rig, root, name)], cellsUnder(root), { sink: () => Effect.void, locker: memoryLocker() })
    const [read] = valueOf(collected)
    expect(read!.cells.get("c1")!.verifications).toEqual([{ name, source: name, answer: "not-checkable", message }])
  })

  it("holds its locks from open to close: a second scope naming one runs none of its steps and is not checkable", async () => {
    const { root, rig } = await samples()
    const locker = memoryLocker()
    const options = { sink: () => Effect.void, locker }
    const second = { ...instanceOf(rig, root, "locked"), label: "locked again", dir: path.join(root, "second") }
    let inner: Exit.Exit<ReadonlyArray<CollectorRun.Collected>, unknown> | undefined
    const body = Effect.gen(function*() {
      inner = (yield* scope([second], cellsUnder(root), options)).collected
    })
    await runScope([instanceOf(rig, root, "locked")], cellsUnder(root), options, body)
    const [refused] = valueOf(inner!)
    expect(refused!.cells.get("c1")!.verifications).toEqual([
      { name: "locked", source: "locked", answer: "not-checkable", message: "its locks not taken: records-chain is held by locked" }
    ])
    expect(traceOf(root)).toEqual(["open locked", "collect locked", "close locked"])
    const third = await runScope([second], cellsUnder(root), options)
    valueOf(third.collected)
    expect(traceOf(root).slice(3)).toEqual(["open locked", "collect locked", "close locked"])
  })

  it("takes a scope's locks once for all its collectors, from the first open to the last close", async () => {
    const { root, rig } = await samples()
    const takes: Array<string> = []
    const memory = memoryLocker()
    const locker: CollectorRun.Locker = (names, holder) => {
      takes.push(`${names.join(" ")} for ${holder}`)
      return memory(names, holder)
    }
    let heldDuring: string | undefined
    const body = Effect.flatMap(Effect.result(memory(["store"], "a probe")), (probed) =>
      Effect.sync(() => {
        heldDuring = Result.isFailure(probed) ? probed.failure : "free"
      }))
    const { collected } = await runScope(
      ["locked", "also-locked", "tidy"].map((name) => instanceOf(rig, root, name)),
      cellsUnder(root),
      { sink: () => Effect.void, locker },
      body
    )
    valueOf(collected)
    expect(takes).toEqual(["records-chain store for locked, also-locked"])
    expect(heldDuring).toBe("store is held by locked, also-locked")
    expect(traceOf(root).filter((line) => /locked$/.test(line))).toEqual([
      "open locked",
      "open also-locked",
      "collect locked",
      "collect also-locked",
      "close also-locked",
      "close locked"
    ])
    const after = await Effect.runPromise(Effect.result(memory(["records-chain", "store"], "the next scope")))
    expect(Result.isSuccess(after)).toBe(true)
  })

  it("runs a locked collector planned twice in one scope, once per part set, under one take of its lock", async () => {
    const { root, rig } = await samples()
    const twice = ["a", "b"].map((part): CollectorRun.Instance => ({
      ...instanceOf(rig, root, "locked"),
      id: `locked-${part}`,
      label: `locked-${part}`,
      dir: path.join(root, "job", "collectors", `locked-${part}`)
    }))
    const { collected } = await runScope(twice, cellsUnder(root), { sink: () => Effect.void, locker: memoryLocker() })
    expect(valueOf(collected).flatMap((one) => [...one.cells.values()].flatMap((answers) => answers.verifications))).toEqual([])
    expect(traceOf(root)).toEqual(["open locked", "open locked", "collect locked", "collect locked", "close locked", "close locked"])
  })
})
