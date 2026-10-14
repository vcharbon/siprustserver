/**
 * The retention pass, on a fixture tree of its own.
 *
 * Three rulings are asserted, and one of them is asserted by actually
 * REMOVING things — the pass exists to delete, and a suite that only ever dry
 * runs proves nothing about the one thing that can lose work. The tree is built
 * under a temp directory per test, never the corpus.
 *
 * The stores are stubs, because none of these claims is about SQL: which rows
 * the queries select is asserted against a real postgres in
 * `@sip/store`'s `abandon.test.ts` and `cuts.test.ts`.
 */
import { NodeServices } from "@effect/platform-node"
import { Cuts, Jobs, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { afterSweep, pass, rebuildCache } from "../../src/housekeeping/cleanup.js"
import { DEFAULT_CLEANUP } from "../../src/knobs.js"
import * as ContentGc from "../../src/content/content-gc.js"
import { CONTENT, FLOWS, LEDGERS } from "../../src/content/content.js"
import * as Reap from "../../src/housekeeping/reap.js"
import * as Retention from "../../src/housekeeping/retention.js"
import { LaneHosting } from "../../src/deployment/index.js"
import type { Options } from "../../src/options.js"
import type { Paths } from "../../src/paths.js"

const HOUR = 3_600_000

/** The lanes the deployment declares on a real clock, beside one paused-clock lane. */
const REAL_LANES = ["real", "managed"]

const hosting = LaneHosting.layerWith({
  lanes: ["paused", ...REAL_LANES],
  lane: (lane) => (REAL_LANES.includes(lane) ? { clock: "real", exclusive: true } : LaneHosting.NEUTRAL_LANE),
  standBatch: () => Effect.succeed(undefined),
  standSut: () => Effect.succeed(undefined),
  preflight: () => Effect.succeed(null)
})

/** The roots of a fixture tree: every one under it, the registry absent. */
const pathsAt = (root: string): Paths => ({
  corpus: path.join(root, "corpus"),
  sessions: root,
  census: path.join(root, "census"),
  registry: path.join(root, "no-registry.json"),
  repoRoot: root
})

let tree: string

beforeEach(() => {
  tree = fs.mkdtempSync(path.join(os.tmpdir(), "cleanup-test-"))
})
afterEach(() => {
  fs.rmSync(tree, { recursive: true, force: true })
})

const refuse = Effect.die(new Error("this test's store was asked the wrong question"))

/** A directory, with its mtime placed `hours` in the past. */
const dirAt = (target: string, hours: number): string => {
  fs.mkdirSync(target, { recursive: true })
  const when = new Date(Date.now() - hours * HOUR)
  fs.utimesSync(target, when, when)
  return target
}

const fileAt = (target: string, hours: number): string => {
  fs.mkdirSync(path.dirname(target), { recursive: true })
  fs.writeFileSync(target, "{}\n")
  const when = new Date(Date.now() - hours * HOUR)
  fs.utimesSync(target, when, when)
  return target
}

/** A bundle as the tree holds it: a run directory with its own index inside. */
const bundle = (stem: string, id: number): string => {
  const dir = path.join(tree, stem, `run${id}`)
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(path.join(dir, "campaign.json"), JSON.stringify({ campaign: `triage-run${id}` }))
  return dir
}

interface Store {
  readonly layer: Layer.Layer<Runs.Service | Cuts.Service | Jobs.Service>
  readonly abandoned: Array<ReadonlyArray<number>>
  /** The floor prep every cache sweep asked the cut store for. */
  readonly floors: Array<number>
}

interface StoreShape {
  readonly protectedBundles?: ReadonlySet<string>
  /** A path alone is a paused-clock run that finished two days ago, unpinned. */
  readonly allBundles?: ReadonlyArray<string | Runs.RecordedBundle>
  readonly unfinished?: ReadonlyArray<Runs.Unfinished>
  readonly abandonable?: ReadonlyArray<Runs.Abandonable>
  readonly standingPrep?: number | null
  readonly live?: Cuts.Live
  /** A store that cannot answer at all, which the automatic hook must survive. */
  readonly broken?: boolean
}

const NOTHING_LIVE: Cuts.Live = { cases: new Set(), flows: new Set(), preps: new Set() }

const store = (shape: StoreShape): Store => {
  const abandoned: Array<ReadonlyArray<number>> = []
  const floors: Array<number> = []
  return {
    abandoned,
    floors,
    layer: Layer.mergeAll(
      Layer.succeed(
        Runs.Service,
        Runs.Service.of({
          open: () => refuse,
          openMany: () => refuse,
          finish: () => refuse,
          importRun: () => refuse,
          lanes: refuse,
          ranOn: () => refuse,
          standings: () => refuse,
          standing: () => refuse,
          protectedBundles: Effect.succeed(shape.protectedBundles ?? new Set()),
          allBundles: Effect.succeed(
            (shape.allBundles ?? []).map((entry, at) =>
              typeof entry === "string"
                ? {
                  runId: at,
                  bundlePath: entry,
                  lane: "paused",
                  finishedAt: new Date(Date.now() - 48 * HOUR).toISOString(),
                  pinned: false,
                  cited: false
                }
                : entry
            )
          ),
          unfinished: shape.broken === true ? refuse : Effect.succeed(shape.unfinished ?? []),
          abandonable: () => Effect.succeed(shape.abandonable ?? []),
          abandon: (ids) =>
            Effect.sync(() => {
              abandoned.push(ids)
              return ids.length
            }),
          unfinishedOf: () => refuse,
          dirty: () => refuse,
          openOfSweep: () => refuse,
          forget: () => refuse
        })
      ),
      // No job runs in these trees: the job pass finds nothing to rule on.
      Layer.succeed(
        Jobs.Service,
        Jobs.Service.of({
          create: () => refuse,
          started: () => refuse,
          close: () => refuse,
          waiting: () => refuse,
          batch: () => refuse,
          sweep: () => refuse,
          stop: () => refuse,
          planned: () => refuse,
          settle: () => refuse,
          show: () => refuse,
          explain: () => refuse,
          list: () => refuse,
          page: () => refuse,
          lineage: () => refuse,
          gone: () => refuse,
          red: () => refuse,
          running: shape.broken === true ? refuse : Effect.succeed([])
        })
      ),
      Layer.succeed(
        Cuts.Service,
        Cuts.Service.of({
          open: () => refuse,
          exists: () => refuse,
          finish: () => refuse,
          record: () => refuse,
          standing: () => refuse,
          resolve: () => refuse,
          cases: () => refuse,
          standingPrep: Effect.succeed(shape.standingPrep === undefined ? 1 : shape.standingPrep),
          latest: refuse,
          captures: () => refuse,
          caseSpans: () => refuse,
          contentFrom: (floor) =>
            Effect.sync(() => {
              floors.push(floor)
              return shape.live ?? NOTHING_LIVE
            }),
          content: () => refuse,
          flowsEnvelope: () => refuse,
          flowsLegs: () => refuse,
          ledger: () => refuse
        })
      )
    )
  }
}

const run = <A, E>(
  effect: Effect.Effect<A, E, Runs.Service | Cuts.Service | Jobs.Service | LaneHosting.Service | FileSystem.FileSystem>,
  on: Store
) =>
  Effect.runPromise(effect.pipe(Effect.provide(Layer.mergeAll(on.layer, hosting, NodeServices.layer))))

/** A retention request over the fixture tree, with the real-clock lanes and a day's grace. */
const ruling = (livePreps: ReadonlySet<number> | null, forgetUnrecorded = false): Retention.PlanRequest => ({
  sessions: tree,
  livePreps,
  realLanes: REAL_LANES,
  graceHours: 24,
  forgetUnrecorded
})

describe("Retention", () => {
  it("does not read the cache or the log trees as a capture's session workdir", async () => {
    // `_content` holds ten thousand hash directories. Walking them for `run<N>`
    // entries costs the pass its whole budget and can never find one.
    dirAt(path.join(tree, CONTENT, "a".repeat(64), "some-case"), 0)
    dirAt(path.join(tree, "_preps", "prep3"), 0)
    const kept = bundle("capture_a", 1)

    const plan = await run(
      Retention.plan(ruling(null)),
      store({ allBundles: [kept], protectedBundles: new Set([kept]) })
    )
    expect(plan.onDisk).toEqual([kept])
    expect(plan.unrecorded).toEqual([])
  })

  it("drops a superseded paused-clock bundle, keeps the current one, and names what the database never saw", async () => {
    const last = bundle("capture_a", 9)
    const gone = bundle("capture_a", 2)
    const unseen = bundle("capture_b", 1)

    const plan = await run(
      Retention.plan(ruling(null)),
      store({ allBundles: [last, gone], protectedBundles: new Set([last]) })
    )
    expect(plan.removable).toEqual([gone])
    expect(plan.unrecorded).toEqual([unseen])
  })

  it("drops a bundle no row names too, once told the store was rebuilt from nothing", async () => {
    const unseen = bundle("capture_b", 1)
    const plan = await run(Retention.plan(ruling(null, true)), store({}))
    expect(plan.removable).toEqual([unseen])
  })

  it("keeps a cut log the store still stands behind, and drops the rest", async () => {
    dirAt(path.join(tree, "_preps", "prep3"), 0)
    dirAt(path.join(tree, "_preps", "prep7"), 0)
    dirAt(path.join(tree, "_preps", "prep9"), 0)
    const on = store({})

    const plan = await run(
      Retention.plan(ruling(new Set([7, 9]))),
      on
    )
    expect(plan.preps).toEqual([path.join(tree, "_preps", "prep3")])
  })

  it("names no cut log at all where the store stands behind nothing", async () => {
    dirAt(path.join(tree, "_preps", "prep3"), 0)
    const plan = await run(
      Retention.plan(ruling(null)),
      store({})
    )
    expect(plan.preps).toEqual([])
  })
})

describe("Reap", () => {
  it("reads a bundle that was never created as never written to", async () => {
    const lastWrite = (bundle: string) => Effect.runPromise(Reap.lastWrite(bundle).pipe(Effect.provide(NodeServices.layer)))
    expect(await lastWrite(path.join(tree, "nothing", "run1"))).toEqual(0)
    expect(await lastWrite(bundle("capture_a", 1))).toBeGreaterThan(0)
  })

  it("closes a row only when the disk agrees with the database", async () => {
    const quiet = bundle("capture_a", 1)
    const busy = bundle("capture_b", 2)
    const never = path.join(tree, "capture_c", "run3")
    const when = new Date(Date.now() - 30 * HOUR)
    fs.utimesSync(quiet, when, when)
    fs.utimesSync(path.join(quiet, "campaign.json"), when, when)

    const on = store({
      abandonable: [
        { id: 1, bundlePath: quiet },
        { id: 2, bundlePath: busy },
        { id: 3, bundlePath: never }
      ]
    })
    const reaped = await run(Reap.reap({ horizonHours: 12, dryRun: false }), on)
    expect(reaped.examined).toEqual(3)
    // The busy bundle is minutes old, so its row is left open whatever the
    // database offered.
    expect(on.abandoned).toEqual([[1, 3]])
    expect(reaped.dead).toEqual([1, 3])
  })

  it("rules on every row and closes none on a dry run", async () => {
    const on = store({ abandonable: [{ id: 1, bundlePath: path.join(tree, "gone", "run1") }] })
    const reaped = await run(Reap.reap({ horizonHours: 12, dryRun: true }), on)
    expect(reaped).toEqual({ examined: 1, dead: [1] })
    expect(on.abandoned).toEqual([])
  })
})

describe("ContentGc", () => {
  const liveHash = "1".repeat(64)
  const deadHash = "2".repeat(64)
  const freshHash = "3".repeat(64)

  const cache = (): void => {
    dirAt(path.join(tree, CONTENT, liveHash), 48)
    dirAt(path.join(tree, CONTENT, deadHash), 48)
    dirAt(path.join(tree, CONTENT, freshHash), 1)
    dirAt(path.join(tree, CONTENT, ".writing-abc"), 48)
    fileAt(path.join(tree, CONTENT, FLOWS, `${liveHash}.json`), 48)
    fileAt(path.join(tree, CONTENT, FLOWS, `${deadHash}.json`), 48)
    dirAt(path.join(tree, CONTENT, LEDGERS, "a".repeat(64)), 48)
  }

  const plan = (
    live: Cuts.Live | null,
    anyOpen = false
  ) => run(ContentGc.plan({ sessions: tree, graceHours: 6, live, anyOpen }), store({}))

  it("collects what no reading cites, and only once it is past the grace period", async () => {
    cache()
    const collected = await plan({
      cases: new Set([liveHash]),
      flows: new Set([liveHash]),
      preps: new Set([1])
    })
    expect(collected.cases).toEqual([path.join(tree, CONTENT, deadHash)])
    expect(collected.flows).toEqual([path.join(tree, CONTENT, FLOWS, `${deadHash}.json`)])
    // An entry younger than the grace is never collected, cited or not: it is
    // the window between the query and the removal.
    expect(ContentGc.collected(collected)).not.toContain(path.join(tree, CONTENT, freshHash))
    expect(collected.scratch).toEqual([path.join(tree, CONTENT, ".writing-abc")])
  })

  it("holds every merged ledger while any run is open", async () => {
    cache()
    const ledger = path.join(tree, CONTENT, LEDGERS, "a".repeat(64))
    const open = await plan(NOTHING_LIVE, true)
    expect(open.ledgers).toEqual([])
    const idle = await plan(NOTHING_LIVE)
    expect(idle.ledgers).toEqual([ledger])
  })

  it("refuses to touch a cache no cut stands behind", async () => {
    cache()
    const collected = await plan(null)
    expect(collected.refused).toEqual(ContentGc.NO_CUT)
    expect(ContentGc.collected(collected)).toEqual([])
  })
})

describe("the pass", () => {
  it("reaps, then drops the superseded bundle, the cache entries and stale cut logs, and keeps the current bundle", async () => {
    const last = bundle("capture_a", 9)
    const gone = bundle("capture_a", 2)
    const deadHash = "2".repeat(64)
    const liveHash = "1".repeat(64)
    dirAt(path.join(tree, CONTENT, deadHash), 48)
    dirAt(path.join(tree, CONTENT, liveHash), 48)
    dirAt(path.join(tree, "_preps", "prep1"), 48)

    const on = store({
      allBundles: [last, gone],
      protectedBundles: new Set([last]),
      abandonable: [{ id: 7, bundlePath: path.join(tree, "capture_z", "run7") }],
      live: { cases: new Set([liveHash]), flows: new Set(), preps: new Set([9]) }
    })
    await run(
      pass({
        paths: pathsAt(tree),
        dryRun: false,
        measure: true,
        horizonHours: 12,
        graceHours: 6,
        keepHours: 24,
        forgetUnrecorded: false
      }),
      on
    )

    expect(on.abandoned).toEqual([[7]])
    expect(fs.existsSync(last)).toBe(true)
    expect(fs.existsSync(gone)).toBe(false)
    expect(fs.existsSync(path.join(tree, CONTENT, liveHash))).toBe(true)
    expect(fs.existsSync(path.join(tree, CONTENT, deadHash))).toBe(false)
    expect(fs.existsSync(path.join(tree, "_preps", "prep1"))).toBe(false)
  })

  /** The pass over an empty tree, with whatever the store was told to answer. */
  const ruled = (on: Store, dryRun = false) =>
    run(
      pass({
        paths: pathsAt(tree),
        dryRun,
        measure: false,
        horizonHours: 12,
        graceHours: 6,
        keepHours: 24,
        forgetUnrecorded: false
      }),
      on
    )

  it("reads the cut at the lowest prep an open run still pins", async () => {
    // The standing prep is the ceiling a fresh command pins; a command still on
    // an older one must keep everything that older reading can name.
    const on = store({ standingPrep: 20, unfinished: [{ id: 3, prepId: 17 }, { id: 4, prepId: 14 }] })
    await ruled(on)
    expect(on.floors).toEqual([14])
  })

  it("rules against the floor the reaping pass moves, not the one it found", async () => {
    // The one open row is a row two days dead; the reaping pass in front closes
    // it, so the pin it was holding the cache down to is not a live ceiling.
    const on = store({
      standingPrep: 20,
      unfinished: [{ id: 3, prepId: 5 }],
      abandonable: [{ id: 3, bundlePath: path.join(tree, "capture_z", "run3") }]
    })
    await ruled(on, true)
    expect(on.floors).toEqual([20])
  })

  it("removes nothing on a dry run", async () => {
    const gone = bundle("capture_a", 2)
    const deadHash = "2".repeat(64)
    dirAt(path.join(tree, CONTENT, deadHash), 48)

    const on = store({
      allBundles: [gone],
      abandonable: [{ id: 7, bundlePath: path.join(tree, "capture_z", "run7") }]
    })
    await run(
      pass({
        paths: pathsAt(tree),
        dryRun: true,
        measure: false,
        horizonHours: 12,
        graceHours: 6,
        keepHours: 24,
        forgetUnrecorded: false
      }),
      on
    )
    expect(on.abandoned).toEqual([])
    expect(fs.existsSync(gone)).toBe(true)
    expect(fs.existsSync(path.join(tree, CONTENT, deadHash))).toBe(true)
  })
})

describe("the automatic hook", () => {
  const options = (universe: Options["selection"]["universe"], cleanup: boolean): Options => ({
    paths: pathsAt(tree),
    lane: "paused",
    selection: {
      universe,
      positional: [],
      onlyNew: false,
      maxSpanSeconds: undefined,
      greenOnLane: undefined,
      sampleSize: undefined,
      limit: undefined,
      cases: []
    },
    batchSize: 1000,
    concurrency: 1,
    startsPerSecond: 0,
    memoryMax: "1G",
    patience: { waitMs: 0 },
    dryRun: false,
    prep: undefined,
    cleanup,
    forceRecut: false
  })

  /** A row nobody will close, which only a pass that RAN would abandon, beside a superseded bundle. */
  const superseded = (): { readonly gone: string; readonly on: Store } => {
    const last = bundle("capture_a", 9)
    const gone = bundle("capture_a", 2)
    return {
      gone,
      on: store({
        allBundles: [last, gone],
        protectedBundles: new Set([last]),
        abandonable: [{ id: 7, bundlePath: path.join(tree, "capture_z", "run7") }]
      })
    }
  }

  it("does not run after an invocation of one capture", async () => {
    const { on } = superseded()
    await run(afterSweep(options("named", true), 1), on)
    expect(on.abandoned).toEqual([])
  })

  it("does not run where the sweep said --no-cleanup", async () => {
    const { on } = superseded()
    await run(afterSweep(options("all", false), 40), on)
    expect(on.abandoned).toEqual([])
  })

  it("runs by default after a campaign, whose captures are named: reaps and drops the superseded bundle", async () => {
    const { gone, on } = superseded()
    await run(afterSweep(options("named", DEFAULT_CLEANUP), 2), on)
    expect(on.abandoned).toEqual([[7]])
    expect(fs.existsSync(gone)).toBe(false)
  })

  it("says what it failed on and leaves the sweep's answer alone", async () => {
    const last = bundle("capture_a", 9)
    const gone = bundle("capture_a", 2)
    const said: Array<string> = []
    const error = console.error
    console.error = (line: string) => void said.push(line)
    try {
      // A store that cannot rule is a pass that does not run, never a sweep
      // that reports differently.
      await run(
        afterSweep(options("all", true), 2),
        store({ allBundles: [last, gone], protectedBundles: new Set([last]), broken: true })
      )
    } finally {
      console.error = error
    }
    expect(said.join("\n")).toContain("the pass after this sweep did not run")
    expect(fs.existsSync(gone)).toBe(true)
  })
})

describe("--rebuild-cache", () => {
  it("removes the whole _content cache and nothing else, ruling on nothing", async () => {
    const kept = bundle("capture_a", 2)
    const cache = dirAt(path.join(tree, CONTENT, "a".repeat(64)), 48)
    const flows = fileAt(path.join(tree, CONTENT, FLOWS, `${"b".repeat(64)}.json`), 48)
    const preps = dirAt(path.join(tree, "_preps", "prep1"), 48)
    await Effect.runPromise(rebuildCache(tree).pipe(Effect.provide(NodeServices.layer)))
    expect(fs.existsSync(path.join(tree, CONTENT))).toBe(false)
    expect(fs.existsSync(cache)).toBe(false)
    expect(fs.existsSync(flows)).toBe(false)
    expect(fs.existsSync(kept)).toBe(true)
    expect(fs.existsSync(preps)).toBe(true)
  })
})
