/**
 * A replay builds before it takes its locks, a lane whose SUT is managed has
 * its SUT built too, and a real-clock lane waits out a build that compiled
 * before its first cell. Its job pre-checks run once the lane is held: a
 * refusal of theirs ends the session before the settle, and the directory of
 * a job-less replay's pre-checks that passed is removed.
 */
import { NodeServices } from "@effect/platform-node"
import { type Lanes, RigPath } from "@sip/rig"
import { Attachments, JobEvents, Sessions, Verifications } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { Collectors, LaneHosting, Roots, Tools } from "../../src/deployment/index.js"
import * as Locks from "../../src/locks.js"
import type { Options } from "../../src/options.js"
import * as ReplayBuild from "../../src/replay/build.js"
import * as CollectedCells from "../../src/replay/collected-cells.js"
import * as PreChecks from "../../src/replay/pre-checks.js"

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
    decides: Effect.die("unused"),
    reading: []
  })
}

const options = (lane: string): Options =>
  ({ lane, patience: { waitMs: 0 }, paths: { sessions: path.join(tree, "sessions") } }) as unknown as Options

/** The stores a job-less replay's pre-checks are never asked to write to. */
const unasked = Layer.mergeAll(Layer.mock(JobEvents.Service, {}), Layer.mock(Verifications.Service, {}), Layer.mock(Attachments.Service, {}))

/** The session run with a build of `buildMs`, the deployment's collectors `collectors`; what happened, in order. */
const run = (lane: string, buildMs: number, collectors: Layer.Layer<Collectors.Service> = Collectors.layer): Promise<ReadonlyArray<string>> => {
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
    Effect.scoped(ReplayBuild.session(options(lane), "test", { seams: { build, sleep } })).pipe(
      Effect.provide(
        Layer.mergeAll(hosting, roots(), tools([], 0), NodeServices.layer, collectors, unasked, Sessions.layer(path.join(tree, "sessions")))
      )
    )
  ).then(() => events)
}

/**
 * The deployment's collectors: on the lane {@link MANAGED}, the job pre-check
 * `lab-own`, whose collect writes to its trace whether the lane's lock is
 * held, then answers `answer`. Its trace's lines with it.
 */
const preChecked = async (answer: "pass" | "fail") => {
  const root = fs.mkdtempSync(path.join(tree, "rig-"))
  const dir = path.join(root, "collectors", "lab-own")
  fs.mkdirSync(dir, { recursive: true })
  const trace = path.join(root, "trace")
  fs.writeFileSync(
    path.join(dir, "collector.toml"),
    `schema = "collector.v1"\nname = "lab-own"\nscope = "job"\nat = "before"\n[run]\ncollect = "./collect"\n`
  )
  fs.writeFileSync(
    path.join(dir, "collect"),
    `#!/bin/sh\n[ -e "${path.join(tree, `lane-${MANAGED}.json`)}" ] && echo "checked under the lane" >> "${trace}"\n` +
      `echo '{"verification": "lab-own", "answer": "${answer}", "message": "the lab is not this one"}'\n`,
    { mode: 0o755 }
  )
  const rig = await Effect.runPromise(RigPath.load({ workspace: root }).pipe(Effect.provide(NodeServices.layer)))
  const lane: Lanes.Loaded = {
    id: MANAGED,
    file: `lanes/${MANAGED}.toml`,
    lane: { schema: "lane.v1", clock: "real" },
    parts: new Map(),
    operations: new Map(),
    capabilities: new Set()
  }
  return {
    layer: Collectors.layerOf(rig, [lane]),
    trace: () => (fs.existsSync(trace) ? fs.readFileSync(trace, "utf8").split("\n").filter((line) => line !== "") : [])
  }
}

/** The directories job-less pre-checks left under the sessions' collectors. */
const leftBehind = (): ReadonlyArray<string> => {
  const dir = path.join(tree, "sessions", CollectedCells.COLLECTORS_DIR)
  return fs.existsSync(dir) ? fs.readdirSync(dir) : []
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

  it("runs the replay's job pre-checks once the lane is held: a fail ends it, refused with the message, before the settle", async () => {
    const checks = await preChecked("fail")
    const ended = await run(MANAGED, 60_000, checks.layer).then(
      (events) => events,
      (cause: unknown) => cause
    )
    expect(ended).toEqual(new PreChecks.Refused({ reason: "the lab is not this one" }))
    expect(checks.trace()).toEqual(["checked under the lane"])
  })

  it("settles once its job pre-checks passed, leaving no directory of theirs behind", async () => {
    const checks = await preChecked("pass")
    expect(await run(MANAGED, 60_000, checks.layer)).toEqual(["built before the locks", `settled ${ReplayBuild.SETTLE_MS}`])
    expect(checks.trace()).toEqual(["checked under the lane"])
    expect(leftBehind()).toEqual([])
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
