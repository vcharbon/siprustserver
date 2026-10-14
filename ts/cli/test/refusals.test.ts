/**
 * The refusals asked before a cut: a missing census run is refused with the
 * facts' hint on where the host states the census root; with a census there,
 * the host's box refusal is the answer.
 */
import { NodeServices } from "@effect/platform-node"
import { Boxing, LaneHosting } from "@sip/corpus"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import type { CliFacts } from "../src/facts.js"
import * as Refusals from "../src/refusals.js"

let dir: string
beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "refusals-"))
})
afterEach(() => {
  fs.rmSync(dir, { recursive: true, force: true })
})

const facts = { censusHint: "CENSUS_ROOT names it" } as CliFacts

const paths = () => ({ corpus: dir, sessions: dir, census: path.join(dir, "census"), registry: dir, repoRoot: dir })

const boxing = Boxing.layerWith({
  box: (request) => ({ command: request.command, args: request.args }),
  refusal: Effect.succeed("no user session"),
  interpreter: Effect.succeed(undefined),
  interpreterVariables: () => ({})
})

const run = <A>(effect: Effect.Effect<A, unknown, Boxing.Service | NodeServices.NodeServices>) =>
  Effect.runPromise(effect.pipe(Effect.provide(Layer.merge(boxing, NodeServices.layer))) as Effect.Effect<A>)

describe("the lane refusal", () => {
  it("names a lane the deployment does not state, and answers null for one it does", async () => {
    const hosting = LaneHosting.layerWith({
      lanes: ["paused"],
      lane: (lane) =>
        lane === "paused"
          ? Effect.succeed(LaneHosting.NEUTRAL_LANE)
          : Effect.fail(new LaneHosting.LaneUnstated({ lane, known: ["paused"] })),
      standBatch: () => Effect.succeed(undefined),
      standSut: () => Effect.succeed(undefined),
      preflight: () => Effect.succeed(null)
    })
    const ask = (lane: string) => Effect.runPromise(Refusals.lane(lane).pipe(Effect.provide(hosting)))
    expect(await ask("paused")).toBeNull()
    expect(await ask("pased")).toBe("there is no lane 'pased': the lanes are paused")
  })
})

describe("the environment refusal", () => {
  const hosting = LaneHosting.layerWith({
    lanes: ["paused", "wall"],
    lane: (lane) =>
      lane === "paused"
        ? Effect.succeed(LaneHosting.NEUTRAL_LANE)
        : lane === "wall"
        ? Effect.succeed({ clock: "real", exclusive: true })
        : Effect.fail(new LaneHosting.LaneUnstated({ lane, known: ["paused", "wall"] })),
    standBatch: () => Effect.succeed(undefined),
    standSut: () => Effect.succeed(undefined),
    preflight: () => Effect.succeed(null)
  })
  /** A deployment whose checkout is not the one wide or real-clock work runs from. */
  const away = { environmentRefusal: (what: string) => `not here: ${what}` } as CliFacts
  const home = { environmentRefusal: (_what: string) => null as string | null } as CliFacts
  const ask = (facts: CliFacts, lane: string, universe: "named" | "known" | "all" | "standing") =>
    Effect.runPromise(Refusals.environment(facts, lane, universe).pipe(Effect.provide(hosting)))

  it("refuses a sweep wider than the captures named, and work on a real-clock lane, away from home", async () => {
    expect(await ask(away, "paused", "all")).toBe("not here: a --all sweep")
    expect(await ask(away, "paused", "known")).toBe("not here: a --known sweep")
    expect(await ask(away, "paused", "standing")).toBe("not here: a --standing sweep")
    expect(await ask(away, "wall", "named")).toBe("not here: work on the real-clock lane wall")
  })

  it("lets named captures on a paused lane run anywhere, and everything run at home", async () => {
    expect(await ask(away, "paused", "named")).toBeNull()
    expect(await ask(away, "pased", "named")).toBeNull()
    expect(await ask(home, "wall", "all")).toBeNull()
  })
})

describe("the cut's refusals", () => {
  it("name where the census root is stated when no census run is there", async () => {
    const refused = await run(Refusals.cut(facts, paths()))
    expect(refused).toMatch(/^no census run at .*; CENSUS_ROOT names it$/)
  })

  it("answer the host's box refusal once a census run is there", async () => {
    fs.mkdirSync(path.join(dir, "census"))
    fs.writeFileSync(path.join(dir, "census", "2026-10-01.census.json"), "{\"runs\":[]}\n")
    expect(await run(Refusals.census(facts, paths()))).toBeNull()
    expect(await run(Refusals.cut(facts, paths()))).toBe("no user session")
  })
})
