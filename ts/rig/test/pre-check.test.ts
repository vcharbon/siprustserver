/**
 * A scope's pre-checks, run as real executables over no cell: a `fail` one
 * answers refuses the scope with its message and a `refused` event, its
 * `close` still run; a `not checkable` refuses nothing and stays the scope's
 * line, but for a required pre-check's, which refuses the scope as does its
 * failed or timed-out collect. An `on-demand` job pre-check runs only for a
 * job that asks for it, and refuses that job as a required one would. A
 * required pre-check a real-clock lane names
 * no part of refuses the scope before any pre-check runs; on a simulated lane
 * it does not.
 */
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as CollectorPlan from "../src/collector-plan.js"
import type * as CollectorRun from "../src/collector-run.js"
import type * as Lanes from "../src/lanes.js"
import * as PreCheck from "../src/pre-check.js"
import * as RigPath from "../src/rig-path.js"
import { memoryLocker, SAMPLES, traceOf } from "./collectors.js"
import { laneOf } from "./operations.js"
import { run, tree } from "./tree.js"

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const STORE = "records-store"

const samples = async () => {
  const root = tree(SAMPLES)
  made.push(root)
  const loaded = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(loaded)) throw new Error(`the samples do not load: ${String(loaded.failure)}`)
  return { root, rig: loaded.success }
}

/** `lane` on the real clock. */
const real = (lane: Lanes.Loaded): Lanes.Loaded => ({ ...lane, lane: { ...lane.lane, clock: "real" } })

/** The batch pre-checks named `names` of lane `lane`, as planned under `root`. */
const batchChecks = (rig: RigPath.RigPath, root: string, lane: Lanes.Loaded, names: ReadonlyArray<string>) =>
  CollectorPlan.batchPlanOf(rig, lane, path.join(root, "batch", "collectors"), path.join(root, "batch"))
    .filter((instance) => PreCheck.isPreCheck(instance) && names.includes(instance.id))

/** A sink collecting each event as `<phase> <collector>: <message>`. */
const collecting = () => {
  const events: Array<string> = []
  const sink: CollectorRun.Sink = (event) => Effect.sync(() => void events.push(`${event.phase} ${event.collector}: ${event.message ?? ""}`))
  return { events, sink }
}

describe("a scope's pre-checks", () => {
  it("refuse the scope with a fail's message and a refused event, close the refusing one, and leave a not-checkable a line", async () => {
    const { root, rig } = await samples()
    const { events, sink } = collecting()
    const instances = batchChecks(rig, root, laneOf("a"), ["refusing-check", "unsure-check"])
    const checked = await Effect.runPromise(PreCheck.run(instances, [], { sink, locker: memoryLocker() }))
    const down = "nothing takes SIP on 192.0.2.1:5060: the target is down"
    expect(checked.refusal).toBe(down)
    expect(events).toContain(`refused refusing-check: ${down}`)
    expect(checked.answers.verifications.map((line) => [line.name, line.answer])).toEqual([
      ["target-up", "fail"],
      ["unsure-check", "not-checkable"]
    ])
    expect(traceOf(root)).toEqual(["collect refusing-check", "collect unsure-check", "close refusing-check"])
  })

  it("refuse nothing where none fails", async () => {
    const { root, rig } = await samples()
    const { events, sink } = collecting()
    const instances = batchChecks(rig, root, laneOf("a"), ["unsure-check"])
    const checked = await Effect.runPromise(PreCheck.run(instances, [], { sink, locker: memoryLocker() }))
    expect(checked.refusal).toBeUndefined()
    expect(checked.answers.verifications[0]?.message).toContain("the lab cannot be asked")
    expect(events.some((event) => event.startsWith("refused"))).toBe(false)
  })
})

describe("a required pre-check", () => {
  /** What the required pre-check `name` comes to on its own: the refusal, its refused events, its lines' answers. */
  const alone = async (name: string) => {
    const { root, rig } = await samples()
    const { events, sink } = collecting()
    const checked = await Effect.runPromise(PreCheck.run(batchChecks(rig, root, laneOf("a"), [name]), [], { sink, locker: memoryLocker() }))
    return {
      refusal: checked.refusal,
      refused: events.filter((event) => event.startsWith("refused")),
      answers: checked.answers.verifications.map((line) => [line.name, line.answer])
    }
  }

  it("refuses its scope where it answers not checkable, saying what it could not check and why", async () => {
    const said = "the required pre-check required-blind could not check lab-reachable: the lab cannot be asked"
    expect(await alone("required-blind")).toEqual({
      refusal: said,
      refused: [`refused required-blind: ${said}`],
      answers: [["lab-reachable", "not-checkable"]]
    })
  })

  it("refuses its scope where its collect fails, quoting the step", async () => {
    const { refusal, refused, answers } = await alone("required-failing")
    expect(refusal).toMatch(/^the required pre-check required-failing could not check required-failing: collect exited 3: the lab cannot be asked/)
    expect(refused).toEqual([`refused required-failing: ${refusal}`])
    expect(answers).toEqual([["required-failing", "not-checkable"]])
  })

  it("refuses its scope where its collect outlives its bound", async () => {
    const { refusal, answers } = await alone("required-hanging")
    expect(refusal).toMatch(/^the required pre-check required-hanging could not check required-hanging: collect /)
    expect(answers).toEqual([["required-hanging", "not-checkable"]])
  }, 10_000)
})

describe("an on-demand pre-check", () => {
  it("runs only for a job that asks for it, and refuses that job where its collect fails, as a required one does", async () => {
    const { root, rig } = await samples()
    const jobChecks = (asked: ReadonlyArray<string>) =>
      CollectorPlan.jobPlanOf(rig, [laneOf("a")], path.join(root, "job", "collectors"), path.join(root, "job"), asked)
        .filter(PreCheck.isPreCheck)
    expect(jobChecks([]).map((instance) => instance.id)).toEqual([])
    const asked = jobChecks(["asked-check"])
    expect(asked.map((instance) => [instance.id, instance.asked])).toEqual([["asked-check", true]])
    const { events, sink } = collecting()
    const checked = await Effect.runPromise(PreCheck.run(asked, [], { sink, locker: memoryLocker() }))
    expect(checked.refusal).toMatch(/^the asked pre-check asked-check could not check asked-check: collect exited 3: the lab cannot be asked/)
    expect(events.filter((event) => event.startsWith("refused"))).toEqual([`refused asked-check: ${checked.refusal}`])
    expect(traceOf(root)).toEqual(["collect asked-check"])
  })
})

describe("a required pre-check a lane names no part of", () => {
  it("refuses on a real-clock lane, before any pre-check runs, and not on a simulated one or one naming the part", async () => {
    const { root, rig } = await samples()
    const lanes = [real(laneOf("bare")), laneOf("simulated"), real(laneOf("stored", { [STORE]: "targets/x.toml" }))]
    const unwired = PreCheck.unwiredOf(rig, lanes)
    expect(unwired.map((one) => [one.collector, one.lane])).toEqual([["required-check", "bare"]])
    const { events, sink } = collecting()
    const instances = batchChecks(rig, root, laneOf("a"), ["refusing-check"])
    const checked = await Effect.runPromise(PreCheck.run(instances, unwired, { sink, locker: memoryLocker() }))
    const said = "bare names no records-store part, which the required pre-check required-check needs (lanes/bare.toml)"
    expect(checked.refusal).toBe(said)
    expect(checked.answers.verifications).toEqual([{ name: "required-check", source: "required-check", answer: "fail", message: said }])
    expect(events).toEqual([`refused required-check: ${said}`])
    expect(traceOf(root)).toEqual([])
  })
})
