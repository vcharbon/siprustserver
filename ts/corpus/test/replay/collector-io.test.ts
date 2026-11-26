/**
 * A scope's collectors under real named box-wide locks (`collector-io.ts`
 * `locker` over `locks.ts`), real executables, the lock root a directory of
 * the case's own: two collectors of two scopes naming one lock run one after
 * the other, open to close; a scope interrupted while its collector waits for
 * a lock another holds runs none of its steps and gives back nothing, the
 * holder's lock kept until its own close; two collectors of one scope share
 * it, taken once.
 *
 * A waiter is seen waiting through the locker's watch, and a holder through
 * its trace: no case waits a fixed time for either.
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
import type * as Locks from "../../src/locks.js"
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

/** How long a collector waits for a lock where the case waits it out on another scope's steps: never reached. */
const WAIT_MS = 120_000

/** How long a collector waits for a lock its own scope took, which it never waits for. */
const SHARED_WAIT_MS = 800

/**
 * Resolves once `holds` does, checking every 20 ms; fails as soon as `fiber`
 * ended first, saying `what` never came. The test's timeout is its only bound.
 */
const until = <A, E>(holds: () => boolean, fiber: Fiber.Fiber<A, E>, what: string): Effect.Effect<void, Error> =>
  Effect.gen(function*() {
    while (!holds()) {
      if (fiber.pollUnsafe() !== undefined) return yield* Effect.fail(new Error(`its scope ended before ${what}`))
      yield* Effect.sleep(20)
    }
  })

/** A watch that settles `waiting` once its locker is told whom it waits for. */
const heardWaiting = (waiting: Deferred.Deferred<void>): Locks.Watch => (who) =>
  who === undefined ? Effect.void : Effect.asVoid(Deferred.succeed(waiting, undefined))

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

/**
 * One scope of `instances` under the box-wide locker, each lock waited for up
 * to `waitMs`, whom it waits for told `watch`: opened, `held` run, collected
 * over no cell, closed; what they collected.
 */
const scopeOf = (instances: ReadonlyArray<CollectorRun.Instance>, held: Effect.Effect<void>, watch?: Locks.Watch, waitMs = WAIT_MS) =>
  Effect.scoped(Effect.gen(function*() {
    const locker = yield* CollectorIo.locker(waitMs, watch)
    const opened = yield* CollectorRun.opened(instances, { sink: () => Effect.void, locker })
    yield* held
    return yield* opened.collect({ cells: [], unread: [], failed: [] })
  }))

describe("collectors naming one box-wide lock", () => {
  it("run one after the other, open to close, across two scopes", async () => {
    const lab = await setup()
    await Effect.runPromise(Effect.gen(function*() {
      const waiting = yield* Deferred.make<void>()
      const holder = yield* Effect.forkChild(scopeOf([lab.alpha], Deferred.await(waiting)))
      yield* until(() => lab.trace().includes("alpha open"), holder, "alpha opened")
      yield* scopeOf([lab.beta], Effect.void, heardWaiting(waiting)).pipe(Effect.ensuring(Deferred.succeed(waiting, undefined)))
      yield* Fiber.join(holder)
    }).pipe(Effect.provide(Layer.merge(lab.roots, NodeServices.layer))))
    expect(lab.trace()).toEqual(["alpha open", "alpha collect", "alpha close", "beta open", "beta collect", "beta close"])
    expect(fs.existsSync(lab.lockFile)).toBe(false)
  })

  it("leave a scope interrupted while it waits running none of its steps, the holder's lock kept until its close", async () => {
    const lab = await setup()
    const seen = await Effect.runPromise(Effect.gen(function*() {
      const go = yield* Deferred.make<void>()
      const holder = yield* Effect.forkChild(scopeOf([lab.alpha], Deferred.await(go)))
      yield* until(() => lab.trace().includes("alpha open"), holder, "alpha opened")
      const waiting = yield* Deferred.make<void>()
      const waiter = yield* Effect.forkChild(scopeOf([lab.beta], Effect.void, heardWaiting(waiting)))
      yield* Effect.raceFirst(Deferred.await(waiting), Effect.asVoid(Fiber.await(waiter)))
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

  it("share it within one scope: both open, collect and close, the lock given back after the last close", async () => {
    const lab = await setup()
    const collected = await Effect.runPromise(
      scopeOf([lab.alpha, lab.beta], Effect.void, undefined, SHARED_WAIT_MS).pipe(Effect.provide(Layer.merge(lab.roots, NodeServices.layer)))
    )
    expect(collected.flatMap((one) => one.scope.verifications)).toEqual([])
    expect(lab.trace()).toEqual(["alpha open", "beta open", "alpha collect", "beta collect", "beta close", "alpha close"])
    expect(fs.existsSync(lab.lockFile)).toBe(false)
  })
})
