/**
 * A replay builds before it takes its locks, a lane whose SUT is managed has
 * its SUT built too, and a real-clock lane waits out a build that compiled
 * before its first cell.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { LaneHosting, Roots, Tools } from "../../src/deployment/index.js"
import * as Locks from "../../src/locks.js"
import type { Options } from "../../src/options.js"
import * as ReplayBuild from "../../src/replay/build.js"

let tree = ""

beforeEach(() => {
  tree = fs.mkdtempSync(path.join(os.tmpdir(), "replay-build-test-"))
})
afterEach(() => {
  fs.rmSync(tree, { recursive: true, force: true })
})

const PAUSED = "paused"
const MANAGED = "managed"

const hosting = LaneHosting.layerWith({
  lanes: [PAUSED, MANAGED],
  lane: (lane) =>
    lane === MANAGED
      ? Effect.succeed({ clock: "real", exclusive: true, sut: "managed" })
      : lane === PAUSED
      ? Effect.succeed(LaneHosting.NEUTRAL_LANE)
      : Effect.fail(new LaneHosting.LaneUnstated({ lane, known: [PAUSED, MANAGED] })),
  standBatch: () => Effect.succeed(undefined),
  standSut: () => Effect.succeed(undefined),
  preflight: () => Effect.succeed(null)
})

const roots = () =>
  Roots.layerWith({
    paths: () => Effect.die("unused"),
    pcapRoots: Effect.succeed([]),
    locks: Effect.succeed(tree),
    bench: Effect.succeed(tree)
  })

/** Tools whose builds compile `ms` each and say which tool they built. */
const tools = (built: Array<string>, ms: number) => {
  let clock = 0
  return Tools.layerWith({
    ready: (tool) =>
      Effect.sync(() => {
        built.push(tool)
        clock += ms
        return `/bin/${tool}`
      }),
    generator: Effect.die("unused"),
    measure: Effect.die("unused"),
    compileMs: Effect.sync(() => clock),
    decides: Effect.die("unused")
  })
}

const options = (lane: string): Options => ({ lane, patience: { waitMs: 0 } }) as unknown as Options

/** The session run with a build of `buildMs`; what happened, in order. */
const run = (lane: string, buildMs: number): Promise<ReadonlyArray<string>> => {
  const events: Array<string> = []
  const build = () =>
    Effect.gen(function*() {
      const quiet = yield* Locks.quiet("probe", { waitMs: 0 })
      events.push(Locks.isTaken(quiet) ? "built before the locks" : "built under the box")
      if (Locks.isTaken(quiet)) yield* quiet.release
      return buildMs
    }).pipe(Effect.provide(Layer.merge(roots(), NodeServices.layer)), Effect.orDie)
  const sleep = (ms: number) => Effect.sync(() => void events.push(`settled ${ms}`))
  return Effect.runPromise(
    Effect.scoped(ReplayBuild.session(options(lane), "test", () => Effect.void, { build, sleep })).pipe(
      Effect.provide(Layer.mergeAll(hosting, roots(), tools([], 0), NodeServices.layer))
    )
  ).then(() => events)
}

describe("ReplayBuild.session", () => {
  it("builds before the box is taken, and a real-clock lane settles after a build that compiled", async () => {
    expect(await run(MANAGED, 60_000)).toEqual(["built before the locks", `settled ${ReplayBuild.SETTLE_MS}`])
  })

  it("refuses a lane the deployment does not state before it builds or takes a lock", async () => {
    const refused = await run("pased", 60_000).then(
      () => "ran",
      (cause: unknown) => String(cause)
    )
    expect(refused).toContain("there is no lane 'pased': the lanes are paused, managed")
  })

  it("does not settle after a build that found the tree current, nor on a paused-clock lane", async () => {
    expect(await run(MANAGED, 800)).toEqual(["built before the locks"])
    expect(await run(PAUSED, 60_000)).toEqual(["built before the locks"])
  })
})

describe("ReplayBuild.forLane", () => {
  const forLane = (lane: string, built: Array<string>, ms: number) =>
    Effect.runPromise(
      ReplayBuild.forLane(lane, () => Effect.void).pipe(Effect.provide(Layer.merge(hosting, tools(built, ms))))
    )

  it("builds the interpreter, and the SUT where the lane manages its own; answers what they compiled for", async () => {
    const managed: Array<string> = []
    expect(await forLane(MANAGED, managed, 3_000)).toBe(6_000)
    expect(managed).toEqual(["interpreter", "sut"])
    const paused: Array<string> = []
    expect(await forLane(PAUSED, paused, 3_000)).toBe(3_000)
    expect(paused).toEqual(["interpreter"])
  })
})
