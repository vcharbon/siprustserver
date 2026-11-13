/**
 * A case's retries: after a lane's batch, each cell that ended `failure` and
 * declares retries is replayed again on that lane, in a batch of its own with
 * the other cells being retried, until it passes or its count runs out. The
 * kept attempt is the first that passes, else the one with the fewest failed
 * checks, the earliest on a tie; it carries every attempt and its bundle.
 */
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { type Cell, type CellResult, errored, type Lane, type Status } from "../src/cell.js"
import type * as Results from "@sip/store/campaign-results"
import { bestOf, type Outcome, PhaseFailed, type Phases, run as runOver } from "../src/run.js"
import { setupOf } from "../src/setup-phase.js"

/** The run of `p` on `lanes`, a pack typing no check of its own. */
const run = <R>(lanes: ReadonlyArray<Lane>, p: Phases<R>) => runOver(lanes, p, [])

const cell = (id: string, retries = 0): Cell => ({
  id,
  ref: `cases/${id}`,
  subcase: "s",
  retries,
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

const check = (n: number): Results.Check => ({
  anchor: "B1 INVITE",
  field: `f${n}`,
  expected: "x",
  seen: "y",
  said: null,
  side: "sut"
})

/** How a cell ends on one attempt: a status, and how many checks failed on a failure. */
interface Ending {
  readonly status: Status
  readonly failed?: number
}

/**
 * Phases whose replay ends each cell as `script(lane, cell id, attempt)` says
 * (pass where it says nothing), recording each batch as `<lane>#<attempt>: <ids>`.
 */
const phases = (
  script: (lane: Lane, id: string, attempt: number) => Ending | undefined,
  options: {
    readonly dirty?: (lane: Lane, attempt: number) => string | undefined
    readonly leftChanged?: (lane: Lane, attempt: number) => string | undefined
    readonly fails?: (lane: Lane, attempt: number) => boolean
  } = {}
) => {
  const batches: Array<string> = []
  const kept: Array<string> = []
  let reported: Outcome | undefined
  const p: Phases<never> = {
    provision: Effect.void,
    setup: setupOf([], () => Effect.void, () => Effect.void),
    compile: Effect.succeed({ cells: [], broken: [] }),
    replay: (lane, cells, attempt) =>
      Effect.suspend(() => {
        batches.push(`${lane}#${attempt}: ${cells.map((c) => c.id).join(",")}`)
        if (options.fails?.(lane, attempt) === true) {
          return Effect.fail(new PhaseFailed({ phase: `replay ${lane}`, reason: "the lane broke" }))
        }
        const leftChanged = options.leftChanged?.(lane, attempt)
        const dirty = options.dirty?.(lane, attempt) ?? leftChanged
        return Effect.succeed({
          ...(leftChanged === undefined ? {} : { leftChanged }),
          results: cells.map((c): CellResult => {
            const ending = script(lane, c.id, attempt) ?? { status: "pass" }
            return {
              ...errored(lane, c, ending.status === "pass" ? "" : `${c.id} ${ending.status}`),
              status: ending.status,
              bundle: `/b/${lane}/${c.id}~${attempt}`,
              checks: ending.status === "failure" ? Array.from({ length: ending.failed ?? 1 }, (_, i) => check(i)) : []
            }
          }),
          ...(dirty === undefined ? {} : { dirty })
        })
      }),
    collectors: (_cells, replays) => Effect.map(replays, (results) => ({ results, leftChanged: [], unanswered: [] })),
    laneKept: (lane, results) =>
      Effect.sync(() => void kept.push(`${lane} after #${batches.length}: ${results.map((r) => r.bundle).join(",")}`)),
    report: (outcome) =>
      Effect.sync(() => {
        reported = outcome
      })
  }
  return { p, batches, kept, reported: () => reported! }
}

const withCells = <R>(p: Phases<R>, cells: ReadonlyArray<Cell>): Phases<R> => ({
  ...p,
  compile: Effect.succeed({ cells, broken: [] })
})

const resultOf = (outcome: Outcome, lane: Lane, id: string): CellResult => {
  const found = outcome.results.filter((r) => r.lane === lane && r.cell.id === id)
  expect(found).toHaveLength(1)
  return found[0]!
}

describe("a case's retries", () => {
  it("replays no cell again by default: a failed cell that declares no retries is judged once", async () => {
    const t = phases((_lane, id) => (id === "a" ? { status: "failure" } : undefined))
    expect(await Effect.runPromise(run(["sim"], withCells(t.p, [cell("a"), cell("b")])))).toBe(1)
    expect(t.batches).toEqual(["sim#1: a,b"])
    const a = resultOf(t.reported(), "sim", "a")
    expect(a.status).toBe("failure")
    expect(a.attempts).toBeUndefined()
  })

  it("replays a failed cell up to its count, each attempt a batch of the cells being retried, on each lane", async () => {
    const t = phases((lane, id) => (id === "c" || (id === "b" && lane === "hosted") ? undefined : { status: "failure" }))
    const cells = [cell("a", 2), cell("b", 1), cell("c", 3), cell("d")]
    expect(await Effect.runPromise(run(["sim", "hosted"], withCells(t.p, cells)))).toBe(1)
    expect(t.batches).toEqual([
      "sim#1: a,b,c,d",
      "sim#2: a,b",
      "sim#3: a",
      "hosted#1: a,b,c,d",
      "hosted#2: a",
      "hosted#3: a"
    ])
    expect(resultOf(t.reported(), "sim", "a").attempts).toHaveLength(3)
    expect(resultOf(t.reported(), "sim", "b").attempts).toHaveLength(2)
    expect(resultOf(t.reported(), "sim", "c").attempts).toBeUndefined()
    expect(resultOf(t.reported(), "sim", "d").attempts).toBeUndefined()
    expect(t.kept).toEqual([
      "sim after #6: /b/sim/a~1,/b/sim/b~1,/b/sim/c~1,/b/sim/d~1",
      "hosted after #6: /b/hosted/a~1,/b/hosted/b~1,/b/hosted/c~1,/b/hosted/d~1"
    ])
    expect(resultOf(t.reported(), "hosted", "b").status).toBe("pass")
    expect(t.reported().results).toHaveLength(8)
  })

  it("hands each lane's kept attempts on once the job's collectors answered, a retry that passed kept over the failure", async () => {
    const t = phases((_lane, id, attempt) => (id === "a" && attempt === 1 ? { status: "failure" } : undefined))
    expect(await Effect.runPromise(run(["sim"], withCells(t.p, [cell("a", 1), cell("b")])))).toBe(0)
    expect(t.batches).toEqual(["sim#1: a,b", "sim#2: a"])
    expect(t.kept).toEqual(["sim after #2: /b/sim/a~2,/b/sim/b~1"])
  })

  it("stops at the first attempt that passes and keeps it", async () => {
    const t = phases((_lane, _id, attempt) => (attempt < 3 ? { status: "failure", failed: 1 } : undefined))
    expect(await Effect.runPromise(run(["sim"], withCells(t.p, [cell("a", 10)])))).toBe(0)
    expect(t.batches).toEqual(["sim#1: a", "sim#2: a", "sim#3: a"])
    const a = resultOf(t.reported(), "sim", "a")
    expect(a.status).toBe("pass")
    expect(a.bundle).toBe("/b/sim/a~3")
    expect(a.attempts?.map((x) => [x.status, x.kept])).toEqual([["failure", false], ["failure", false], ["pass", true]])
  })

  it("keeps the attempt with the fewest failed checks when every one fails, the earliest on a tie", async () => {
    const failed = [3, 1, 2, 1]
    const t = phases((_lane, _id, attempt) => ({ status: "failure", failed: failed[attempt - 1]! }))
    expect(await Effect.runPromise(run(["sim"], withCells(t.p, [cell("a", 3)])))).toBe(1)
    expect(t.batches).toHaveLength(4)
    const a = resultOf(t.reported(), "sim", "a")
    expect(a.status).toBe("failure")
    expect(a.bundle).toBe("/b/sim/a~2")
    expect(a.checks).toHaveLength(1)
    expect(a.attempts?.map((x) => [x.failed, x.kept])).toEqual([[3, false], [1, true], [2, false], [1, false]])
  })

  it("never replays an errored or skipped cell again", async () => {
    const t = phases((_lane, id) => (id === "e" ? { status: "error" } : id === "s" ? { status: "skipped" } : undefined))
    expect(await Effect.runPromise(run(["sim"], withCells(t.p, [cell("e", 10), cell("s", 10)])))).toBe(2)
    expect(t.batches).toEqual(["sim#1: e,s"])
    expect(resultOf(t.reported(), "sim", "e").attempts).toBeUndefined()
    expect(resultOf(t.reported(), "sim", "s").attempts).toBeUndefined()
  })

  it("carries the count of attempts and every attempt's bundle on the kept result", async () => {
    const t = phases((_lane, _id, attempt) => (attempt === 1 ? { status: "failure", failed: 2 } : undefined))
    await Effect.runPromise(run(["sim"], withCells(t.p, [cell("a", 10)])))
    expect(resultOf(t.reported(), "sim", "a").attempts).toEqual([
      { bundle: "/b/sim/a~1", status: "failure", failed: 2, kept: false },
      { bundle: "/b/sim/a~2", status: "pass", failed: 0, kept: true }
    ])
  })

  it("judges a retry's batch end as the first one's: a dirty end is an error of the run naming the attempt", async () => {
    const t = phases((_lane, _id, attempt) => (attempt === 1 ? { status: "failure" } : undefined), {
      dirty: (_lane, attempt) => (attempt === 2 ? "b2bua_active_calls last read 1" : undefined)
    })
    expect(await Effect.runPromise(run(["sim"], withCells(t.p, [cell("a", 1)])))).toBe(2)
    expect(t.reported().errors).toEqual([
      { phase: "replay sim", message: "attempt 2: batch end: b2bua_active_calls last read 1" }
    ])
  })

  it("keeps the best failure where a retry's batch could not run, the lane's error naming the attempt", async () => {
    const t = phases(() => ({ status: "failure", failed: 2 }), { fails: (_lane, attempt) => attempt === 2 })
    expect(await Effect.runPromise(run(["sim"], withCells(t.p, [cell("a", 5)])))).toBe(2)
    expect(t.batches).toEqual(["sim#1: a", "sim#2: a"])
    const a = resultOf(t.reported(), "sim", "a")
    expect(a.status).toBe("failure")
    expect(a.bundle).toBe("/b/sim/a~1")
    expect(a.attempts?.map((x) => [x.status, x.kept])).toEqual([["failure", true], ["error", false]])
    expect(t.reported().errors).toEqual([{ phase: "replay sim", message: "attempt 2: the lane broke" }])
  })

  it("runs no retry on a lane whose batch left the lab changed: each cell it would retry has an attempt not run, an environment error", async () => {
    const changed = "the step kill for a left the lab changed: 2 heal attempts failed"
    const t = phases((lane, id) => (lane === "sim" && id === "b" ? undefined : { status: "failure" }), {
      leftChanged: (lane, attempt) => (lane === "sim" && attempt === 1 ? changed : undefined)
    })
    expect(await Effect.runPromise(run(["sim", "other"], withCells(t.p, [cell("a", 3), cell("b", 3)])))).toBe(2)
    expect(t.batches, "no retry on the changed lab; the other lane's run").toEqual([
      "sim#1: a,b",
      "other#1: a,b",
      "other#2: a,b",
      "other#3: a,b",
      "other#4: a,b"
    ])
    const a = resultOf(t.reported(), "sim", "a")
    expect(a.status).toBe("failure")
    expect(a.attempts?.map((x) => [x.status, x.kept])).toEqual([["failure", true], ["error", false]])
    expect(resultOf(t.reported(), "sim", "b").status).toBe("pass")
    expect(t.reported().errors).toEqual([{ phase: "replay sim", message: `batch end: ${changed}` }])
  })
})

describe("the best attempt", () => {
  const result = (status: Status, failed = 0): CellResult => ({
    ...errored("sim", cell("a", 10), ""),
    status,
    checks: Array.from({ length: failed }, (_, i) => check(i))
  })

  it("is the first that passes", () => {
    expect(bestOf([result("failure", 1), result("pass"), result("pass")])).toBe(1)
  })

  it("is a skipped attempt (every check it could make passed) over any failure", () => {
    expect(bestOf([result("failure", 1), result("skipped")])).toBe(1)
  })

  it("is the failure with the fewest failed checks, the earliest on a tie, never an errored attempt", () => {
    expect(bestOf([result("failure", 2), result("error"), result("failure", 1), result("failure", 1)])).toBe(2)
  })
})
