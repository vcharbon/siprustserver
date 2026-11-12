import type { OperationRun } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as fs from "node:fs"
import { afterEach, describe, expect, it } from "vitest"
import { type Cell, type CellResult, errored, type Lane, type Status } from "../src/cell.js"
import { PhaseFailed, type Outcome, type Phases, run as runOver } from "../src/run.js"
import { setupOf } from "../src/setup-phase.js"
import { planned, traceOf, workspace } from "./setup-operations.js"

/** The run of `p` on `lanes`, a pack typing no check of its own. */
const run = <R>(lanes: ReadonlyArray<Lane>, p: Phases<R>) => runOver(lanes, p, [])

const cell = (id: string): Cell => ({
  id,
  ref: `Routing/${id}`,
  subcase: "s",
  retries: 0,
  source: { file: `cases/${id}.ts`, text: "" },
  checked: [],
  locks: [],
  needs: [],
  tags: [],
  alone: false,
  heal: { healMs: 0, healRealClockOnSimulatedMs: 0 },
  dir: `/cells/${id}`,
  endpoints: ["alice", "bob"],
  egress: "bob"
})

/** Phases recording their order; `statuses` says how each lane's cells end, `fails` which phase fails. */
const phases = (options: {
  readonly cells?: ReadonlyArray<Cell>
  readonly broken?: ReadonlyArray<{ readonly file: string; readonly absolute: string; readonly reason: string }>
  readonly statuses?: Partial<Record<Lane, Status>>
  readonly fails?: ReadonlyArray<string>
  readonly dirty?: ReadonlyArray<Lane>
  /** The setup operations the run takes, their events recorded in the phases' order; none where absent. */
  readonly setup?: ReadonlyArray<OperationRun.Instance>
  /** A lane whose replay never ends: the run is interrupted there. */
  readonly hangs?: Lane
  /** The job's own lines its collectors answered that did not pass. */
  readonly unanswered?: ReadonlyArray<string>
}) => {
  const order: Array<string> = []
  let reported: Outcome | undefined
  const step = (name: string) =>
    Effect.suspend(() => {
      order.push(name)
      return options.fails?.includes(name) ? Effect.fail(new PhaseFailed({ phase: name, reason: `${name} broke` })) : Effect.void
    })
  const p: Phases<never> = {
    provision: step("provision"),
    setup: setupOf(
      options.setup ?? [],
      (event) => Effect.sync(() => void order.push(`${event.phase} ${event.operation}`)),
      (left) => Effect.sync(() => void order.push(`recorded left changed: ${left.join(", ")}`))
    ),
    compile: Effect.andThen(
      step("compile"),
      Effect.succeed({ cells: options.cells ?? [cell("a"), cell("b")], broken: options.broken ?? [] })
    ),
    replay: (lane, cells) =>
      Effect.andThen(
        Effect.andThen(step(`replay ${lane}`), lane === options.hangs ? Effect.never : Effect.void),
        Effect.succeed({
          results: cells.map((c): CellResult => ({
            ...errored(lane, c, ""),
            status: options.statuses?.[lane] ?? "pass"
          })),
          ...(options.dirty?.includes(lane) ? { dirty: "b2bua_active_calls last read 1" } : {})
        })
      ),
    collectors: (_cells, replays) =>
      Effect.andThen(
        Effect.sync(() => void order.push("collectors")),
        Effect.map(replays, (results) => ({ results, leftChanged: [], unanswered: options.unanswered ?? [] }))
      ),
    laneKept: () => Effect.void,
    report: (outcome) =>
      Effect.andThen(
        Effect.sync(() => {
          reported = outcome
        }),
        step("report")
      )
  }
  return { p, order, reported: () => reported! }
}

describe("a campaign run", () => {
  it("compiles, provisions once, opens the job's collectors, replays each lane, then reports: 0 when every cell passes", async () => {
    const t = phases({})
    expect(await Effect.runPromise(run(["sim", "lab", "hosted"], t.p))).toBe(0)
    expect(t.order).toEqual(["compile", "provision", "collectors", "replay sim", "replay lab", "replay hosted", "report"])
    expect(t.reported().results).toHaveLength(6)
  })

  it("exits 1 on a failed cell and 2 on an errored one, error first", async () => {
    expect(await Effect.runPromise(run(["sim", "hosted"], phases({ statuses: { hosted: "failure" } }).p))).toBe(1)
    expect(await Effect.runPromise(run(["sim"], phases({ statuses: { sim: "error" } }).p))).toBe(2)
    expect(
      await Effect.runPromise(run(["sim", "hosted"], phases({ statuses: { sim: "error", hosted: "failure" } }).p))
    ).toBe(2)
  })

  it("runs every other cell when a case file is broken, and errors the run naming the file", async () => {
    const t = phases({ broken: [{ file: "campaigns/bad.ts", absolute: "/ws/campaigns/bad.ts", reason: "does not typecheck: x" }] })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(2)
    expect(t.order).toEqual(["compile", "provision", "collectors", "replay sim", "report"])
    expect(t.reported().results.map((r) => [r.cell.id, r.status])).toEqual([["a", "pass"], ["b", "pass"]])
    expect(t.reported().errors).toEqual([{ phase: "compile campaigns/bad.ts", message: "does not typecheck: x" }])
  })

  it("exits 0 on skipped cells (not runnable, not checkable), which are never a pass and never an error", async () => {
    expect(await Effect.runPromise(run(["sim", "hosted"], phases({ statuses: { hosted: "skipped" } }).p))).toBe(0)
    expect(await Effect.runPromise(run(["sim", "hosted"], phases({ statuses: { sim: "skipped", hosted: "failure" } }).p)))
      .toBe(1)
  })

  it("stops at a provisioning that fails: no call, an error reported, 2", async () => {
    const t = phases({ fails: ["provision"] })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(2)
    expect(t.order).toEqual(["compile", "provision", "report"])
    expect(t.reported().errors.map((e) => e.phase)).toEqual(["provision"])
  })

  it("is an error when no case compiles to a cell", async () => {
    const t = phases({ cells: [] })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(2)
    expect(t.order).toEqual(["compile", "report"])
  })

  it("is an error when a line the job's collectors answered of the job did not pass, its cells kept", async () => {
    const t = phases({ unanswered: ["db-dump (db-dump) not-checkable: the dump was not taken: pg_dump exited 1"] })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(2)
    expect(t.reported().results.map((r) => r.status)).toEqual(["pass", "pass"])
    expect(t.reported().errors).toEqual([
      { phase: "collectors", message: "db-dump (db-dump) not-checkable: the dump was not taken: pg_dump exited 1" }
    ])
  })

  it("is an error when a batch ends with the SUT still holding a call no cell was charged with", async () => {
    const t = phases({ dirty: ["lab"] })
    expect(await Effect.runPromise(run(["lab"], t.p))).toBe(2)
    expect(t.reported().errors).toEqual([
      { phase: "replay lab", message: "batch end: b2bua_active_calls last read 1" }
    ])
  })

  it("errors every cell of a lane whose replay could not run, and goes on with the next lane", async () => {
    const t = phases({ fails: ["replay hosted"] })
    expect(await Effect.runPromise(run(["hosted", "sim"], t.p))).toBe(2)
    expect(t.order).toEqual(["compile", "provision", "collectors", "replay hosted", "replay sim", "report"])
    const hosted = t.reported().results.filter((r) => r.lane === "hosted")
    expect(hosted.map((r) => [r.status, r.message])).toEqual([
      ["error", "replay hosted broke"],
      ["error", "replay hosted broke"]
    ])
  })

  it("is an error when the report could not be written, whatever the cells came to", async () => {
    const t = phases({ fails: ["report"] })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(2)
  })
})

describe("a campaign run's setup operations", () => {
  const made: Array<string> = []
  afterEach(() => {
    for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
  })
  const sampled = async (names: ReadonlyArray<string>) => {
    const root = workspace()
    made.push(root)
    return { root, setup: await planned(root, names) }
  }
  const HEALED = ["heal restart-worker", "healed restart-worker", "recovered restart-worker"]

  it("injects after provisioning and heals after the report of a run that failed", async () => {
    const { root, setup } = await sampled(["restart-worker"])
    const t = phases({ setup, statuses: { sim: "failure" } })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(1)
    expect(t.order).toEqual([
      "compile",
      "provision",
      "inject restart-worker",
      "injected restart-worker",
      "collectors",
      "replay sim",
      "report",
      ...HEALED
    ])
    expect(traceOf(root)).toEqual(["inject restart-worker", "heal restart-worker"])
  })

  it("heals a run interrupted while it replays, the interruption kept", async () => {
    const { root, setup } = await sampled(["restart-worker"])
    const t = phases({ setup, hangs: "sim" })
    const exit = await Effect.runPromise(Effect.gen(function*() {
      const fiber = yield* Effect.forkChild(run(["sim"], t.p))
      yield* Effect.sleep("20 millis").pipe(Effect.repeat({ until: () => t.order.includes("replay sim") }))
      return yield* Fiber.interrupt(fiber).pipe(Effect.andThen(Fiber.await(fiber)))
    }))
    expect(Exit.hasInterrupts(exit)).toBe(true)
    expect(t.order.slice(-4)).toEqual(["replay sim", ...HEALED])
    expect(traceOf(root)).toEqual(["inject restart-worker", "heal restart-worker"])
  })

  it("exits 2 when a heal fails twice, every cell having passed, and records it on the run once healed", async () => {
    const { root, setup } = await sampled(["restart-worker", "stuck"])
    const t = phases({ setup })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(2)
    expect(t.order.filter((step) => step.endsWith(" stuck")).slice(2)).toEqual([
      "heal stuck",
      "failed stuck",
      "heal stuck",
      "failed stuck",
      "lab-left-changed stuck",
      "recorded left changed: stuck"
    ])
    expect(t.order.indexOf("report")).toBeLessThan(t.order.indexOf("heal stuck"))
    expect(t.order.at(-1)).toBe("recorded left changed: stuck")
    expect(traceOf(root).slice(-3)).toEqual(["heal stuck", "heal stuck", "heal restart-worker"])
  })

  it("records a lab left changed on a run interrupted while it replays", async () => {
    const { setup } = await sampled(["stuck"])
    const t = phases({ setup, hangs: "sim" })
    const exit = await Effect.runPromise(Effect.gen(function*() {
      const fiber = yield* Effect.forkChild(run(["sim"], t.p))
      yield* Effect.sleep("20 millis").pipe(Effect.repeat({ until: () => t.order.includes("replay sim") }))
      return yield* Fiber.interrupt(fiber).pipe(Effect.andThen(Fiber.await(fiber)))
    }))
    expect(Exit.hasInterrupts(exit)).toBe(true)
    expect(t.order.slice(-2)).toEqual(["lab-left-changed stuck", "recorded left changed: stuck"])
  })

  it("runs no call when an inject fails: the setup healed, then the error reported, 2", async () => {
    const { root, setup } = await sampled(["restart-worker", "broken"])
    const t = phases({ setup })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(2)
    expect(t.order).not.toContain("replay sim")
    expect(t.order.slice(-1)).toEqual(["report"])
    expect(traceOf(root)).toEqual(["inject restart-worker", "inject broken", "heal broken", "heal restart-worker"])
    expect(t.reported().errors.map((e) => [e.phase, e.message.split(",")[0]])).toEqual([["setup", "broken: inject exited 3"]])
  })

  it("records a lab left changed by an earlier heal of a failed inject before the report closes the lanes", async () => {
    const { setup } = await sampled(["stuck", "broken"])
    const t = phases({ setup })
    expect(await Effect.runPromise(run(["sim"], t.p))).toBe(2)
    expect(t.order.slice(-3)).toEqual(["lab-left-changed stuck", "recorded left changed: stuck", "report"])
  })
})
