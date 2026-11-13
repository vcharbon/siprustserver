/**
 * One cell's operation steps on a hosted lane: an inject its run asks is the
 * operation the lane binds by that name, answered once it did its work and
 * once the lane's system stood again; a failed inject is refused with its
 * words and still healed at the cell's end; a heal failing twice is the event
 * `lab-left-changed`, the step's label what the batch's end states; a name the
 * lane binds nothing to is refused.
 */
import { NodeServices } from "@effect/platform-node"
import type { SutHandle } from "@sip/corpus/replay/batch-protocol"
import { type Lanes, type OperationRun, RigPath } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { leftChangedOf, stepsOf } from "../src/cell-steps.js"
import { traceOf, workspace } from "./setup-operations.js"

const made: Array<string> = []
afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const CELL = { id: "alice-calls-bob", case: "alice calls bob", lane: "lab", claims: {} }

/** A lane `lab` of the sample workspace binding each sample operation by its own name. */
const laneOf = async () => {
  const root = workspace()
  made.push(root)
  const rig = await Effect.runPromise(RigPath.load({ workspace: root }).pipe(Effect.provide(NodeServices.layer)))
  const lane: Lanes.Loaded = {
    id: "lab",
    file: path.join(root, "lanes", "lab.toml"),
    lane: { schema: "lane.v1", clock: "real" },
    parts: new Map(),
    operations: rig.operations,
    capabilities: new Set()
  }
  return { root, lane }
}

const collecting = () => {
  const events: Array<OperationRun.Event> = []
  const sink: OperationRun.Sink = (event) => Effect.sync(() => void events.push(event))
  return { events, sink, phases: () => events.map((event) => `${event.phase} ${event.operation}`) }
}

/** A system that stands again on `restand`, counting each time it was asked. */
const counting = () => {
  let asked = 0
  const sut: SutHandle = { metrics: undefined, died: () => undefined, stop: Effect.void, restand: Effect.sync(() => void asked++) }
  return { sut, asked: () => asked }
}

const ASK = { spec: "/cells/alice.json", node: "i1" }

describe("a cell's operation steps", () => {
  it("inject the bound operation, stand the system again before answering, and heal it once the cell ended", async () => {
    const { root, lane } = await laneOf()
    const { sink, phases } = collecting()
    const { sut, asked } = counting()
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, sut)
    const answered = await Effect.runPromise(Effect.result(steps.injector({ ...ASK, action: "operation:restart-worker" })))
    expect(Result.isSuccess(answered)).toBe(true)
    expect(asked(), "stood again before the answer").toBe(1)
    expect(await Effect.runPromise(steps.healed)).toEqual([])
    expect(traceOf(root)).toEqual(["inject restart-worker", "heal restart-worker"])
    const label = "restart-worker for alice-calls-bob"
    expect(phases()).toEqual([`inject ${label}`, `injected ${label}`, `heal ${label}`, `healed ${label}`, `recovered ${label}`])
    expect(JSON.parse(fs.readFileSync(path.join(root, "cell", "alice-calls-bob-i1", "context.json"), "utf8")).planned).toEqual([CELL])
  })

  it("refuse a failed inject with its words, and heal it all the same", async () => {
    const { root, lane } = await laneOf()
    const { sink, phases } = collecting()
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, counting().sut)
    const answered = await Effect.runPromise(Effect.flip(steps.injector({ ...ASK, action: "operation:broken" })))
    expect(answered).toContain("broken for alice-calls-bob: inject exited 3")
    expect(await Effect.runPromise(steps.healed)).toEqual([])
    expect(traceOf(root)).toEqual(["inject broken", "heal broken"])
    expect(phases().map((line) => line.split(" ")[0])).toEqual(["inject", "failed", "heal", "healed", "recovered"])
  })

  it("leave the lab changed where a heal fails twice, which the batch's end states", async () => {
    const { root, lane } = await laneOf()
    const { sink, phases } = collecting()
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, counting().sut)
    await Effect.runPromise(steps.injector({ ...ASK, action: "operation:stuck" }))
    const left = await Effect.runPromise(steps.healed)
    expect(left).toEqual(["stuck for alice-calls-bob"])
    expect(phases().map((line) => line.split(" ")[0])).toEqual(["inject", "injected", "heal", "failed", "heal", "failed", "lab-left-changed"])
    expect(leftChangedOf(left)).toBe("the step stuck for alice-calls-bob left the lab changed: 2 heal attempts failed")
  })

  it("refuse an operation the lane binds nothing to, nothing run", async () => {
    const { root, lane } = await laneOf()
    const { sink, events } = collecting()
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, undefined)
    expect(await Effect.runPromise(Effect.flip(steps.injector({ ...ASK, action: "operation:reboot" })))).toBe(
      "the lane lab binds no operation reboot"
    )
    expect(await Effect.runPromise(steps.healed)).toEqual([])
    expect(events).toEqual([])
  })
})
