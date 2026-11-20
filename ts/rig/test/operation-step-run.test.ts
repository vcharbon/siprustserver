/**
 * An operation run as a step of one cell: its context names the cell's
 * scope and the cell, its parameters the manifest's after the lane binding's;
 * its heal settles what the lane's orchestrator owns between `heal` and its
 * probe, a settle that fails failing the attempt, twice the lab left changed.
 */
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import type * as Context from "../src/context.js"
import * as OperationRun from "../src/operation-run.js"
import * as RigPath from "../src/rig-path.js"
import { laneOf, SAMPLES, traceOf } from "./operations.js"
import { run, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const samples = async () => {
  const root = tree(SAMPLES)
  made.push(root)
  const loaded = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(loaded)) throw new Error(`the samples do not load: ${String(loaded.failure)}`)
  return { root, rig: loaded.success }
}

const collecting = () => {
  const events: Array<OperationRun.Event> = []
  const sink: OperationRun.Sink = (event) => Effect.sync(() => void events.push(event))
  return { events, sink, phases: () => events.map((event) => event.phase) }
}

const CELL: Context.Planned = { id: "alice-calls-bob", case: "alice calls bob", lane: "lab", claims: {} }

describe("an operation as a step of a cell", () => {
  it("injects with the cell's scope, the cell, and the lane binding's parameters over the manifest's", async () => {
    const { root, rig } = await samples()
    const instance: OperationRun.Instance = {
      operation: rig.operations.get("stateful")!,
      label: "stateful for alice-calls-bob",
      lane: laneOf("lab", { "records-store": "targets/records-store/a.toml" }),
      dir: path.join(root, "cells", "alice-calls-bob-i1"),
      cell: CELL,
      params: { grace_s: 1 }
    }
    const { sink, phases } = collecting()
    await Effect.runPromise(OperationRun.inject(instance, sink))
    const seen = JSON.parse(fs.readFileSync(path.join(root, "context-seen.json"), "utf8")) as Context.Context
    expect([seen.scope, seen.planned, seen.params]).toEqual(["cell", [CELL], { grace_s: 1 }])
    expect(phases()).toEqual(["inject", "injected"])
  })

  it("settles the lane between heal and its probe, a settle that fails twice leaving the lab changed", async () => {
    const { root, rig } = await samples()
    const instance = (name: string): OperationRun.Instance => ({
      operation: rig.operations.get(name)!,
      label: name,
      lane: undefined,
      dir: path.join(root, "cells", name)
    })
    const settled = Effect.sync(() => fs.appendFileSync(path.join(root, "trace"), "settled\n"))
    const healed = collecting()
    expect(await Effect.runPromise(OperationRun.heal(instance("first"), healed.sink, settled))).toBe(true)
    expect(traceOf(root)).toEqual(["heal first", "settled"])
    expect(healed.phases()).toEqual(["heal", "healed", "recovered"])
    const unsettled = collecting()
    const recovered = await Effect.runPromise(OperationRun.heal(instance("second"), unsettled.sink, Effect.fail("the worker did not stand")))
    expect(recovered).toBe(false)
    expect(unsettled.phases()).toEqual(["heal", "failed", "heal", "failed", "lab-left-changed"])
    expect(unsettled.events[1]?.message).toBe("heal: the worker did not stand")
  })
})
