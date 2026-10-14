/**
 * The deployment seams' defaults: every default layer builds, and a seam a
 * guess would make dangerous — roots, tools, the anonymiser, the cell host, a
 * job's entry point, the tracker's texts — refuses by name when a command
 * reaches for it; the others state nothing (no lane declared, no side row, no
 * repository, every ticket missing). A deployment's own layer is what a
 * reader is answered from.
 */
import { NodeServices } from "@effect/platform-node"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import {
  Boxing,
  CaseLayout,
  CellHost,
  Extraction,
  Identity,
  LaneHosting,
  Launcher,
  Roots,
  Tools,
  Tracker
} from "../../src/deployment/index.js"

const defaults = Layer.mergeAll(
  Boxing.layer,
  CaseLayout.layer,
  CellHost.layer,
  Extraction.layer,
  Identity.layer,
  LaneHosting.layer,
  Launcher.layer,
  Roots.layer,
  Tools.layer,
  Tracker.layer
)

/** A default seam, built. */
const seam = <S>(service: Effect.Effect<S, never, Layer.Success<typeof defaults>>) =>
  Effect.runPromise(Effect.provide(service, defaults))

/** What `effect` refuses with, or undefined where it succeeds. */
const refusal = async <A, E>(effect: Effect.Effect<A, E>) => {
  const exit = await Effect.runPromiseExit(effect)
  return Exit.isFailure(exit) ? String(Cause.squash(exit.cause)) : undefined
}

describe("the seams a guess would make dangerous", () => {
  it("build, and refuse by name when a command reaches for them", async () => {
    const roots = await seam(Roots.Service)
    const tools = await seam(Tools.Service)
    const extraction = await seam(Extraction.Service)
    const host = await seam(CellHost.Service)
    expect(await refusal(roots.paths({}))).toContain("Roots.layerWith")
    expect(await refusal(roots.locks)).toContain("Roots.layerWith")
    expect(await refusal(tools.ready("interpreter"))).toContain("Tools.layerWith")
    expect(await refusal(tools.generator)).toContain("Tools.layerWith")
    expect(await refusal(extraction.anonymise({ raw: "r", dir: "d", artifacts: "a", capture: "c" })))
      .toContain("Extraction.layerWith")
    expect(await refusal(host.tests)).toContain("CellHost.layerWith")
    expect(await refusal(Layer.build(host.services({ interpreter: "i", audit: "a" })).pipe(Effect.scoped, Effect.provide(NodeServices.layer))))
      .toContain("CellHost.layerWith")
  })

  it("refuse a job launch, naming the kind, and run no hook", async () => {
    const launcher = await seam(Launcher.Service)
    expect(await refusal(launcher.entry("cut"))).toContain("no entry point runs a cut job")
    expect(await Effect.runPromise(launcher.hooks)).toEqual([])
  })

  it("refuse the tracker's texts, so a pass keeping what tickets cite never reads 'nothing cited'", async () => {
    const tracker = await seam(Tracker.Service)
    expect(await refusal(tracker.texts(null))).toContain("Tracker.layerWith")
    expect([...(await Effect.runPromise(tracker.states(["p/1", "nope"])))]).toEqual([["p/1", "missing"], ["nope", "missing"]])
  })
})

describe("the seams that state nothing by default", () => {
  it("declare no lane: every lane is a paused-clock lane with nothing stood and nothing to check", async () => {
    const hosting = await seam(LaneHosting.Service)
    expect(hosting.lane("any")).toEqual(LaneHosting.NEUTRAL_LANE)
    expect(LaneHosting.realClock(hosting)).toEqual([])
    expect(await Effect.runPromise(hosting.standBatch("any", { under: "/x", memoryMax: "1G", cells: 1 }))).toBeUndefined()
    expect(await Effect.runPromise(hosting.standSut("any", "/x"))).toBeUndefined()
    expect(await Effect.runPromise(hosting.preflight("any"))).toBeNull()
  })

  it("lay out no side row that states a call key or reaches the interpreter", async () => {
    const layout = await seam(CaseLayout.Service)
    expect(layout.rowFile("7")).toBe("7.json")
    expect(layout.callKeyOf({ number: "7", document: { anchor: "a@b" } })).toBeUndefined()
    expect(layout.interpreterVariable).toBeUndefined()
  })

  it("box nothing: a child runs as it is, on any host, and a cell execs the interpreter itself", async () => {
    const boxing = await seam(Boxing.Service)
    expect(boxing.box({ memoryMax: "1G", cpus: 2, command: "gen", args: ["a"] })).toEqual({ command: "gen", args: ["a"] })
    expect(await Effect.runPromise(boxing.refusal)).toBeNull()
    expect(await Effect.runPromise(boxing.interpreter)).toBeUndefined()
  })

  it("name no environment and no repository", async () => {
    const identity = await seam(Identity.Service)
    expect([identity.environment, identity.repositories]).toEqual([null, []])
  })

  it("read a cell as its gating charges standing and no rule hit of its own", async () => {
    const host = await seam(CellHost.Service)
    const findings = [{ gating: true }, { gating: false }] as unknown as Parameters<CellHost.Interface["charges"]>[1]
    expect(host.charges("/cell", findings)).toEqual([findings[0]])
  })
})

describe("a deployment's own layer", () => {
  it("answers what the deployment states", async () => {
    const hosting = LaneHosting.layerWith({
      lanes: ["fast", "lab"],
      lane: (lane) => (lane === "lab" ? { clock: "real", exclusive: true, cellCeiling: 4 } : LaneHosting.NEUTRAL_LANE),
      standBatch: () => Effect.succeed(undefined),
      standSut: () => Effect.succeed(undefined),
      preflight: (lane) => Effect.succeed(lane === "lab" ? "the lab is down" : null)
    })
    const read = await Effect.runPromise(Effect.provide(LaneHosting.Service, hosting))
    expect(LaneHosting.realClock(read)).toEqual(["lab"])
    expect(await Effect.runPromise(read.preflight("lab"))).toBe("the lab is down")
  })
})
