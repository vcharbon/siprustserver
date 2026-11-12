/**
 * An on-demand run of one collector over a landed cell, a real executable
 * against a real postgres: its attachment lands on the cell, its lines leave
 * the cell's own as they stand; a collector the job's level does not set
 * on-demand is refused before anything runs. And the level a job names
 * (`Collectors.Level`) deciding what a batch plans.
 *
 * The suite builds and drops its OWN database (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { type Lanes, RigPath } from "@sip/rig"
import { Attachments, Client, Runs, Sessions, Verifications } from "@sip/store"
import { CURRENT } from "@sip/store/test-identity"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { Collectors, Roots } from "../../src/deployment/index.js"
import * as OnDemand from "../../src/replay/on-demand.js"

const SCRATCH = await scratch("corpus_on_demand_test")
const TREE = fs.mkdtempSync(path.join(os.tmpdir(), "on-demand-"))
const LANE = "lane-a"
const COLLECTION = { deep: { always: ["asked"] } }

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  fs.rmSync(TREE, { recursive: true, force: true })
  if (!served) return
  await drop(SCRATCH)
}, 120_000)

/** A rig path with the batch collector `asked`, on-demand by default, which attaches a log to each cell it reads and answers a failing line. */
const deployment = async () => {
  const root = fs.mkdtempSync(path.join(TREE, "rig-"))
  const dir = path.join(root, "collectors", "asked")
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(
    path.join(dir, "collector.toml"),
    `schema = "collector.v1"\nname = "asked"\nscope = "batch"\ndefault = "on-demand"\n[run]\ncollect = "./collect"\n`
  )
  fs.writeFileSync(
    path.join(dir, "collect"),
    [
      "#!/bin/sh",
      `echo "asked now" > "$SIPRIG_OUT/c1.log"`,
      `echo '{"attachment":{"kind":"log","title":"asked now","file":"c1.log"},"cell":"c1"}'`,
      `echo '{"verification":"asked","cell":"c1","answer":"fail"}'`
    ].join("\n"),
    { mode: 0o755 }
  )
  const rig = await Effect.runPromise(RigPath.load({ workspace: root }).pipe(Effect.provide(NodeServices.layer)))
  const lane: Lanes.Loaded = {
    id: LANE,
    file: `lanes/${LANE}.toml`,
    lane: { schema: "lane.v1", clock: "simulated" },
    parts: new Map(),
    operations: new Map(),
    capabilities: new Set()
  }
  return { collectors: Collectors.layerOf(rig, [lane], COLLECTION), lane }
}

const roots = () => {
  const locks = fs.mkdtempSync(path.join(TREE, "locks-"))
  return Roots.layerWith({
    paths: () => Effect.die("unused"),
    pcapRoots: Effect.succeed([]),
    locks: Effect.succeed(locks),
    bench: Effect.succeed(locks)
  })
}

type Services = Attachments.Service | Collectors.Service | Roots.Service | Runs.Service | Verifications.Service | FileSystem.FileSystem

const withStore = <A, E>(collectors: Layer.Layer<Collectors.Service>, effect: Effect.Effect<A, E, Services>): Promise<A> =>
  Effect.runPromise(
    effect.pipe(
      Effect.provide(Layer.mergeAll(Attachments.layer, Runs.layer, Verifications.layer)),
      Effect.provide(Layer.mergeAll(collectors, roots(), Sessions.layer(TREE), NodeServices.layer)),
      Effect.provide(Client.layerFromUrl(urlFor(SCRATCH)))
    ) as Effect.Effect<A, E>
  )

/** A run whose one cell `case-1` landed passing, its bundle stating its clock and window. */
const landedCell = Effect.gen(function*() {
  const runs = yield* Runs.Service
  const bundle = fs.mkdtempSync(path.join(TREE, "run-"))
  const dir = path.join(bundle, "c1")
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(path.join(dir, "run-config.json"), `{"claim_numbers":{},"clock":"virtual","identities":{},"lane":"${LANE}","route_target":"127.0.0.1:5080"}\n`)
  fs.writeFileSync(path.join(dir, "timing.json"), `{"settle_budget_ms":100,"settled_at_ms":10,"started_at_ms":0,"wall_started_at_ms":1,"wall_ended_at_ms":2}\n`)
  const opened = yield* runs.open({ capture: "asked.pcap", lane: LANE, mode: "record", sessionDir: bundle, identity: CURRENT })
  yield* runs.finish({
    id: opened.id,
    status: "done",
    cases: [{
      caseId: "case-1",
      passed: true,
      verdict: "ok",
      skipped: null,
      failedStep: null,
      rfcGating: null,
      mainError: null,
      classification: null,
      document: null,
      verifications: [{ name: "call", source: "interpreter", answer: "pass" }]
    }]
  })
  return { id: "c1", caseId: "case-1", lane: LANE, dir, runId: opened.id }
})

describe.skipIf(!served)("an on-demand run", () => {
  it("lands its attachment on the cell and leaves the cell's lines as they stand", async () => {
    const { collectors } = await deployment()
    const { ran, attached, lines } = await withStore(collectors, Effect.gen(function*() {
      const cell = yield* landedCell
      const ran = yield* OnDemand.run({ collector: "asked", cells: [cell], dir: path.join(TREE, "asked-1") })
      const holder = { scope: "cell", runId: cell.runId, caseId: cell.caseId } as const
      return {
        ran,
        attached: yield* Effect.flatMap(Attachments.Service, (store) => store.of(holder)),
        lines: yield* Effect.flatMap(Verifications.Service, (store) => store.of(holder))
      }
    }))
    expect(attached.map((one) => [one.name, one.kind, one.title, one.source])).toEqual([["asked", "log", "asked now", "asked"]])
    expect(lines).toEqual([{ name: "call", source: "interpreter", answer: "pass" }])
    expect(ran.cells[0]!.answers.verifications).toEqual([{ name: "asked", source: "asked", answer: "fail" }])
    expect(ran.leftChanged).toEqual([])
  })

  it("is refused where the job's level sets the collector always, and runs nothing", async () => {
    const { collectors } = await deployment()
    const refused = await withStore(collectors, Effect.gen(function*() {
      const cell = yield* landedCell
      return yield* OnDemand.run({ collector: "asked", cells: [cell], dir: path.join(TREE, "asked-2") }).pipe(
        Effect.flip,
        Effect.provideService(Collectors.Level, "deep")
      )
    }))
    expect(refused.reason).toBe("asked is set always, not on-demand")
    expect(fs.existsSync(path.join(TREE, "asked-2"))).toBe(false)
  })
})

describe("the level a job names", () => {
  it("decides the collectors a batch plans: one the level sets always runs, its default on-demand does not", async () => {
    const { collectors } = await deployment()
    const plan = (level: string | undefined) =>
      Effect.runPromise(Effect.gen(function*() {
        const seam = yield* Collectors.Service
        return (yield* seam.batch(LANE, "/b", "/b")).map((instance) => instance.id)
      }).pipe(Effect.provideService(Collectors.Level, level), Effect.provide(collectors)))
    expect(await plan(undefined)).toEqual([])
    expect(await plan("deep")).toEqual(["asked"])
  })
})
