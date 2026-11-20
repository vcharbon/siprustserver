/**
 * One cell's operation steps on a hosted lane: an inject its run asks is the
 * operation the lane binds by that name, answered once it did its work and
 * once the lane's system stood again; a failed inject, or a system not stood
 * again, is refused with its words and still healed at the cell's end; a heal
 * failing twice is the event `lab-left-changed`, the step's label what the
 * batch's end states; a name the lane binds nothing to is refused. The system
 * is claimed stopped by a step only for the process an inject of an operation
 * that `stops` it named; a death no step claimed is the event `system-died`.
 *
 * The operations, the system's stand and the events all write one trace, in
 * the order they happen.
 */
import { NodeServices } from "@effect/platform-node"
import type { Restood, SutHandle } from "@sip/corpus/replay/batch-protocol"
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

/** The pid the sample `stopper` names stopped, and `named-stopped` names without stating it `stops` the system. */
const STOPPED = 4242

/** `name`, an operation stating `stops` or not, whose inject names {@link STOPPED} stopped, written under `root`. */
const stopping = (root: string, name: string, stops: boolean) => {
  const dir = path.join(root, "operations", name)
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(
    path.join(dir, "operation.toml"),
    `schema = "operation.v1"\nname = "${name}"\n${stops ? `stops = "system"\n` : ""}[run]\ninject = "./inject"\nheal = "./heal"\n`
  )
  fs.writeFileSync(
    path.join(dir, "inject"),
    `#!/bin/sh\necho "inject ${name}" >> ../../trace\necho ${STOPPED} > "$SIPRIG_OUT/stopped"\n`,
    { mode: 0o755 }
  )
  fs.writeFileSync(path.join(dir, "heal"), `#!/bin/sh\necho "heal ${name}" >> ../../trace\n`, { mode: 0o755 })
}

/** A lane `lab` of the sample workspace, `stopper` and `named-stopped` added, binding each operation by its own name. */
const laneOf = async () => {
  const root = workspace()
  made.push(root)
  stopping(root, "stopper", true)
  stopping(root, "named-stopped", false)
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

/** `line` appended to the trace of the workspace `root`. */
const traced = (root: string, line: string) => fs.appendFileSync(path.join(root, "trace"), `${line}\n`)

/** A sink tracing each event's phase, and the events it took. */
const collecting = (root: string) => {
  const events: Array<OperationRun.Event> = []
  const sink: OperationRun.Sink = (event) =>
    Effect.sync(() => {
      events.push(event)
      traced(root, `event ${event.phase}`)
    })
  return { events, sink, phases: () => events.map((event) => event.phase) }
}

/**
 * A system whose `restand` traces the pid it was asked to claim (`-` for
 * none), then answers the next of `answers` (stood nothing, by default), or
 * fails with it where it is a string.
 */
const standing = (root: string, answers: ReadonlyArray<Restood | string | undefined> = []) => {
  let asked = 0
  const sut: SutHandle = {
    metrics: undefined,
    died: () => undefined,
    stop: Effect.void,
    restand: (claim) =>
      Effect.suspend(() => {
        traced(root, `restand ${claim ?? "-"}`)
        const answer = answers[asked++]
        return typeof answer === "string" ? Effect.fail(answer) : Effect.succeed(answer)
      })
  }
  return sut
}

const ASK = { ask: "1", spec: "/cells/alice.json", node: "i1" }

describe("a cell's operation steps", () => {
  it("inject the bound operation, stand the system again after its work and before answering, and heal it once the cell ended", async () => {
    const { root, lane } = await laneOf()
    const { sink } = collecting(root)
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, standing(root))
    const answered = await Effect.runPromise(Effect.result(steps.injector({ ...ASK, action: "operation:restart-worker" })))
    expect(Result.isSuccess(answered)).toBe(true)
    expect(await Effect.runPromise(steps.healed)).toEqual([])
    expect(traceOf(root)).toEqual([
      "event inject",
      "inject restart-worker",
      "restand -",
      "event injected",
      "event heal",
      "heal restart-worker",
      "restand -",
      "event healed",
      "event recovered"
    ])
    expect(JSON.parse(fs.readFileSync(path.join(root, "cell", "alice-calls-bob-i1", "context.json"), "utf8")).planned).toEqual([CELL])
  })

  it("claim the death of the process an operation that stops the system named stopped, at its inject only", async () => {
    const { root, lane } = await laneOf()
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), collecting(root).sink, standing(root))
    await Effect.runPromise(steps.injector({ ...ASK, action: "operation:stopper" }))
    await Effect.runPromise(steps.healed)
    expect(traceOf(root).filter((line) => line.startsWith("restand"))).toEqual([`restand ${STOPPED}`, "restand -"])
  })

  it("claim no death for an operation that names a process stopped but does not state it stops the system", async () => {
    const { root, lane } = await laneOf()
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), collecting(root).sink, standing(root))
    await Effect.runPromise(steps.injector({ ...ASK, action: "operation:named-stopped" }))
    expect(traceOf(root).filter((line) => line.startsWith("restand"))).toEqual(["restand -"])
  })

  it("record a death of the system no step stopped as the step's event system-died, with its exit", async () => {
    const { root, lane } = await laneOf()
    const { events, sink, phases } = collecting(root)
    const died = { exit: "the stand-in worker exited 101 — its log is /x.log", claimed: false }
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, standing(root, [undefined, died]))
    await Effect.runPromise(steps.injector({ ...ASK, action: "operation:stopper" }))
    expect(await Effect.runPromise(steps.healed)).toEqual([])
    expect(phases()).toEqual(["inject", "injected", "heal", "system-died", "healed", "recovered"])
    expect(events.find((event) => event.phase === "system-died")?.message).toBe(`no step stopped it: ${died.exit}`)
  })

  it("record no event for the death of the process the step named stopped", async () => {
    const { root, lane } = await laneOf()
    const { sink, phases } = collecting(root)
    const claimed = { exit: "the stand-in worker exited SIGKILL — its log is /x.log", claimed: true }
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, standing(root, [claimed]))
    await Effect.runPromise(steps.injector({ ...ASK, action: "operation:stopper" }))
    await Effect.runPromise(steps.healed)
    expect(phases()).toEqual(["inject", "injected", "heal", "healed", "recovered"])
  })

  it("refuse an inject whose system is not stood again, a failed event its reason, and heal it all the same", async () => {
    const { root, lane } = await laneOf()
    const { events, sink, phases } = collecting(root)
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, standing(root, ["the worker not stood again: no port"]))
    const refused = await Effect.runPromise(Effect.flip(steps.injector({ ...ASK, action: "operation:restart-worker" })))
    expect(refused).toContain("the worker not stood again: no port")
    expect(phases()).toEqual(["inject", "failed"])
    expect(events[1]?.message).toBe("inject: the worker not stood again: no port")
    expect(await Effect.runPromise(steps.healed)).toEqual([])
    expect(phases()).toEqual(["inject", "failed", "heal", "healed", "recovered"])
  })

  it("refuse a failed inject with its words, and heal it all the same", async () => {
    const { root, lane } = await laneOf()
    const { sink, phases } = collecting(root)
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, standing(root))
    const answered = await Effect.runPromise(Effect.flip(steps.injector({ ...ASK, action: "operation:broken" })))
    expect(answered).toContain("broken for alice-calls-bob: inject exited 3")
    expect(await Effect.runPromise(steps.healed)).toEqual([])
    expect(traceOf(root).filter((line) => !line.startsWith("event"))).toEqual(["inject broken", "heal broken", "restand -"])
    expect(phases()).toEqual(["inject", "failed", "heal", "healed", "recovered"])
  })

  it("leave the lab changed where a heal fails twice, which the batch's end states", async () => {
    const { root, lane } = await laneOf()
    const { sink, phases } = collecting(root)
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, standing(root))
    await Effect.runPromise(steps.injector({ ...ASK, action: "operation:stuck" }))
    const left = await Effect.runPromise(steps.healed)
    expect(left).toEqual(["stuck for alice-calls-bob"])
    expect(phases()).toEqual(["inject", "injected", "heal", "failed", "heal", "failed", "lab-left-changed"])
    expect(leftChangedOf(left)).toBe("the step stuck for alice-calls-bob left the lab changed: 2 heal attempts failed")
  })

  it("refuse an operation the lane binds nothing to, nothing run", async () => {
    const { root, lane } = await laneOf()
    const { sink, events } = collecting(root)
    const steps = stepsOf(lane, CELL, path.join(root, "cell"), sink, undefined)
    expect(await Effect.runPromise(Effect.flip(steps.injector({ ...ASK, action: "operation:reboot" })))).toBe(
      "the lane lab binds no operation reboot"
    )
    expect(await Effect.runPromise(steps.healed)).toEqual([])
    expect(events).toEqual([])
  })
})
