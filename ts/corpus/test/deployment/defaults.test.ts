/**
 * The deployment seams' defaults: a seam a guess would make dangerous —
 * roots, tools, the anonymiser, the cell host, a job's entry point — refuses
 * by name until a deployment states it; the others state nothing (no lane
 * declared, no side row, no repository, no ticket). A deployment's own layer
 * is what a reader is answered from.
 */
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import type * as Layer from "effect/Layer"
import { describe, expect, it } from "vitest"
import {
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

/** The message a service's default layer fails with, or undefined where it builds. */
const refusalOf = async <I, S, E>(service: Effect.Effect<S, never, I>, layer: Layer.Layer<I, E>) => {
  const exit = await Effect.runPromiseExit(Effect.provide(service, layer))
  return Exit.isFailure(exit) ? String(Cause.squash(exit.cause)) : undefined
}

describe("the seams a guess would make dangerous", () => {
  it("refuse by name until a deployment states them", async () => {
    expect(await refusalOf(Roots.Service, Roots.layer)).toContain("Roots.layerWith")
    expect(await refusalOf(Tools.Service, Tools.layer)).toContain("Tools.layerWith")
    expect(await refusalOf(Extraction.Service, Extraction.layer)).toContain("Extraction.layerWith")
    expect(await refusalOf(CellHost.Service, CellHost.layer)).toContain("CellHost.layerWith")
  })

  it("refuse a job launch, naming the kind, and run no hook", async () => {
    const launcher = await Effect.runPromise(Effect.provide(Launcher.Service, Launcher.layer))
    const exit = await Effect.runPromiseExit(launcher.entry("cut"))
    expect(Exit.isFailure(exit) && String(Cause.squash(exit.cause))).toContain("no entry point runs a cut job")
    expect(await Effect.runPromise(launcher.hooks)).toEqual([])
  })
})

describe("the seams that state nothing by default", () => {
  it("declare no lane: every lane is a paused-clock lane with nothing stood and nothing to check", async () => {
    const hosting = await Effect.runPromise(Effect.provide(LaneHosting.Service, LaneHosting.layer))
    expect(hosting.lane("any")).toEqual(LaneHosting.NEUTRAL_LANE)
    expect(LaneHosting.realClock(hosting)).toEqual([])
    expect(await Effect.runPromise(hosting.standBatch("any", { under: "/x", memoryMax: "1G", cells: 1 }))).toBeUndefined()
    expect(await Effect.runPromise(hosting.standSut("any", "/x"))).toBeUndefined()
    expect(await Effect.runPromise(hosting.preflight("any"))).toBeNull()
  })

  it("lay out no side row that states a call key or reaches the interpreter", async () => {
    const layout = await Effect.runPromise(Effect.provide(CaseLayout.Service, CaseLayout.layer))
    expect(layout.rowFile("7")).toBe("7.json")
    expect(layout.callKeyOf({ number: "7", document: { anchor: "a@b" } })).toBeUndefined()
    expect(layout.interpreterVariable).toBeUndefined()
  })

  it("name no environment and no repository", async () => {
    const identity = await Effect.runPromise(Effect.provide(Identity.Service, Identity.layer))
    expect([identity.environment, identity.repositories]).toEqual([null, []])
  })

  it("hold no ticket: every reference is missing, no ticket has a text", async () => {
    const tracker = await Effect.runPromise(Effect.provide(Tracker.Service, Tracker.layer))
    expect([...(await Effect.runPromise(tracker.states(["p/1", "nope"])))]).toEqual([["p/1", "missing"], ["nope", "missing"]])
    expect(await Effect.runPromise(tracker.texts(null))).toEqual([])
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
