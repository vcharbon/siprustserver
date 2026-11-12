/**
 * A scope's collectors under real named box-wide locks (`collector-io.ts`
 * `locker` over `locks.ts`), real executables, the lock root a directory of
 * the case's own: two collectors of two scopes naming one lock run one after
 * the other, open to close; a scope interrupted while its collector waits for
 * a lock another holds runs none of its steps and gives back nothing, the
 * holder's lock kept until its own close.
 *
 * Locks poll on the real clock (`Locks` POLL_MS): a waiter is seen waiting
 * after one poll.
 */
import { NodeServices } from "@effect/platform-node"
import { CollectorPlan, CollectorRun, type Lanes, RigPath } from "@sip/rig"
import * as Deferred from "effect/Deferred"
import * as Effect from "effect/Effect"
import * as Fiber from "effect/Fiber"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { Roots } from "../../src/deployment/index.js"
import * as CollectorIo from "../../src/replay/collector-io.js"

const TREE = fs.mkdtempSync(path.join(os.tmpdir(), "collector-io-"))
afterAll(() => fs.rmSync(TREE, { recursive: true, force: true }))

const LANE: Lanes.Loaded = {
  id: "lane-a",
  file: "lanes/lane-a.toml",
  lane: { schema: "lane.v1", clock: "simulated" },
  parts: new Map(),
  operations: new Map(),
  capabilities: new Set()
}

/** More than one poll of a lock taken by another. */
const ONE_POLL_MS = 800

/**
 * A rig of the batch collectors `alpha` and `beta`, each naming the lock
 * `lab`, each step appending `<name> <step>` to the trace; the lock root, and
 * the instance of each for a batch of its own.
 */
const setup = async () => {
  const root = fs.mkdtempSync(path.join(TREE, "case-"))
  const trace = path.join(root, "trace")
  for (const name of ["alpha", "beta"]) {
    const dir = path.join(root, "rig", "collectors", name)
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(
      path.join(dir, "collector.toml"),
      `schema = "collector.v1"\nname = "${name}"\nscope = "batch"\nlocks = ["lab"]\n` +
        `[run]\nopen = "./open"\ncollect = "./collect"\nclose = "./close"\n`
    )
    for (const step of ["open", "collect", "close"]) {
      fs.writeFileSync(path.join(dir, step), `#!/bin/sh\necho "${name} ${step}" >> "${trace}"\n`, { mode: 0o755 })
    }
  }
  const rig = await Effect.runPromise(RigPath.load({ workspace: path.join(root, "rig") }).pipe(Effect.provide(NodeServices.layer)))
  const instanceOf = (name: string): CollectorRun.Instance => {
    const holder = path.join(root, `batch-${name}`)
    return CollectorPlan.batchPlanOf(rig, LANE, path.join(holder, "collectors"), holder).find((one) => one.collector.name === name)!
  }
  const locks = path.join(root, "locks")
  fs.mkdirSync(locks)
  return {
    alpha: instanceOf("alpha"),
    beta: instanceOf("beta"),
    lockFile: path.join(locks, `${CollectorIo.LOCK_PREFIX}lab.json`),
    roots: Roots.layerWith({
      paths: () => Effect.die("unused"),
      pcapRoots: Effect.succeed([]),
      locks: Effect.succeed(locks),
      bench: Effect.succeed(locks)
    }),
    trace: () => (fs.existsSync(trace) ? fs.readFileSync(trace, "utf8").split("\n").filter((line) => line !== "") : [])
  }
}

/** One scope of `instance` under the box-wide locker: opened, `held` run, collected over no cell, closed. */
const scopeOf = (instance: CollectorRun.Instance, held: Effect.Effect<void>) =>
  Effect.scoped(Effect.gen(function*() {
    const locker = yield* CollectorIo.locker(10_000)
    const opened = yield* CollectorRun.opened([instance], { sink: () => Effect.void, locker })
    yield* held
    yield* opened.collect([])
  }))

describe("collectors naming one box-wide lock", () => {
  it("run one after the other, open to close, across two scopes", async () => {
    const lab = await setup()
    await Effect.runPromise(
      Effect.all([
        scopeOf(lab.alpha, Effect.sleep(ONE_POLL_MS)),
        Effect.andThen(Effect.sleep(100), scopeOf(lab.beta, Effect.void))
      ], { concurrency: 2 }).pipe(Effect.provide(Layer.merge(lab.roots, NodeServices.layer)))
    )
    expect(lab.trace()).toEqual(["alpha open", "alpha collect", "alpha close", "beta open", "beta collect", "beta close"])
    expect(fs.existsSync(lab.lockFile)).toBe(false)
  })

  it("leave a scope interrupted while it waits running none of its steps, the holder's lock kept until its close", async () => {
    const lab = await setup()
    const seen = await Effect.runPromise(Effect.gen(function*() {
      const go = yield* Deferred.make<void>()
      const holder = yield* Effect.forkChild(scopeOf(lab.alpha, Deferred.await(go)))
      yield* Effect.sleep(200)
      const waiter = yield* Effect.forkChild(scopeOf(lab.beta, Effect.void))
      yield* Effect.sleep(ONE_POLL_MS)
      yield* Fiber.interrupt(waiter)
      const afterInterrupt = { trace: lab.trace(), lock: fs.readFileSync(lab.lockFile, "utf8") }
      yield* Deferred.succeed(go, undefined)
      yield* Fiber.join(holder)
      return { afterInterrupt, trace: lab.trace(), lockLeft: fs.existsSync(lab.lockFile) }
    }).pipe(Effect.provide(Layer.merge(lab.roots, NodeServices.layer))))
    expect(seen.afterInterrupt.trace).toEqual(["alpha open"])
    expect(seen.afterInterrupt.lock).toContain("collector alpha")
    expect(seen.trace).toEqual(["alpha open", "alpha collect", "alpha close"])
    expect(seen.lockLeft).toBe(false)
  })
})
