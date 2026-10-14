/**
 * Which locks a replay takes is what decides how many of them a box runs at
 * once, so the sessions are asserted against each other: a paused-clock
 * replay shares the box, a singleton lane excludes even another replay on
 * itself, and a real-clock lane excludes every replay on the box. A session is
 * a resource of the scope it is taken in, so a scope that fails gives its
 * locks back.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as Layer from "effect/Layer"
import * as Scope from "effect/Scope"
import * as TestClock from "effect/testing/TestClock"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { LaneHosting, Roots } from "../src/deployment/index.js"
import type * as Locks from "../src/locks.js"
import * as Session from "../src/session.js"

/** A paused-clock lane anyone shares, and a real-clock singleton. */
const PAUSED = "paused"
const SINGLE = "single"

let root: string

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "session-test-"))
})
afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

const services = () =>
  Layer.mergeAll(
    Roots.layerWith({
      paths: () => Effect.die("unused"),
      pcapRoots: Effect.succeed([]),
      locks: Effect.succeed(root),
      bench: Effect.succeed(root)
    }),
    LaneHosting.layerWith({
      lanes: [PAUSED, SINGLE],
      lane: (lane) => Effect.succeed(lane === SINGLE ? { clock: "real", exclusive: true } : LaneHosting.NEUTRAL_LANE),
      standBatch: () => Effect.succeed(undefined),
      standSut: () => Effect.succeed(undefined),
      preflight: () => Effect.succeed(null)
    }),
    NodeServices.layer
  )

const options = (lane: string, waitMs = 0) => ({ lane, patience: { waitMs } })

/** A session held open past the call, so a later one can be asked against it. */
interface Open {
  readonly release: () => Promise<void>
}

/** The session taken into a scope of its own, held until `release`. */
const taken = async (lane: string, what: string): Promise<Open> => {
  const scope = await Effect.runPromise(Scope.make())
  await Effect.runPromise(Session.forReplay(options(lane), what).pipe(Scope.provide(scope), Effect.provide(services())))
  return { release: () => Effect.runPromise(Scope.close(scope, Exit.void)) }
}

/** The session refused: who held it. */
const held = async (lane: string, what: string): Promise<string> => {
  const exit = await Effect.runPromiseExit(
    Effect.scoped(Session.forReplay(options(lane), what)).pipe(Effect.provide(services()))
  )
  expect(Exit.isFailure(exit)).toBe(true)
  const why = Exit.isFailure(exit) ? String(exit.cause) : ""
  expect(why).toContain("Session.Held")
  return why
}

describe("a paused-clock lane", () => {
  it("lets two replays run together", async () => {
    const a = await taken(PAUSED, "replay A")
    const b = await taken(PAUSED, "replay B")
    await a.release()
    await b.release()
  })
})

describe("a singleton lane", () => {
  it("takes itself alone even between two replays", async () => {
    const first = await taken(SINGLE, "replay A")
    expect(await held(SINGLE, "replay B")).toContain("replay A")
    await first.release()
    await (await taken(SINGLE, "replay B")).release()
  })
})

describe("a real-clock lane", () => {
  it("holds the box against a paused-clock replay, and names itself", async () => {
    const real = await taken(SINGLE, "replay real")
    expect(await held(PAUSED, "replay paused")).toContain("replay real")
    await real.release()
    await (await taken(PAUSED, "replay paused")).release()
  })

  it("waits for the paused-clock replays already loading the box", async () => {
    const paused = await taken(PAUSED, "replay paused")
    expect(await held(SINGLE, "replay real")).toContain("replay paused")
    // The refused replay gave the quiet back, so the box still takes readers.
    await (await taken(PAUSED, "replay paused B")).release()
    await paused.release()
    await (await taken(SINGLE, "replay real")).release()
  })
})

describe("a session as a resource", () => {
  it("gives the lane back when the scope it was taken in fails", async () => {
    const exit = await Effect.runPromiseExit(
      Effect.scoped(Effect.andThen(Session.forReplay(options(SINGLE), "replay that fails"), Effect.fail("the drive failed")))
        .pipe(Effect.provide(services()))
    )
    expect(Exit.isFailure(exit)).toBe(true)
    // Nobody holds the lane: the next replay on it is taken, not refused.
    await (await taken(SINGLE, "replay after")).release()
  })

  it("gives the lane back when the box refuses it", async () => {
    const paused = await taken(PAUSED, "replay paused")
    expect(await held(SINGLE, "replay real")).toContain("replay paused")
    await paused.release()
    // The lane the refused replay had taken first is free again.
    await (await taken(SINGLE, "replay real")).release()
  })
})

describe("a session that waits", () => {
  it("two paused-clock jobs take the box at once, neither told to wait", async () => {
    const heard: Array<string | undefined> = []
    const watch: Locks.Watch = (holder) => Effect.sync(() => void heard.push(holder))
    await Effect.runPromise(
      Effect.scoped(
        Effect.gen(function*() {
          yield* Session.forReplay(options(PAUSED), "job 1", watch)
          yield* Session.forReplay(options(PAUSED), "job 2", watch)
        })
      ).pipe(Effect.provide(services()))
    )
    expect(heard).toEqual([])
  })

  it("tells its watcher who holds the lane, then that it has it", async () => {
    const first = await taken(SINGLE, "replay A")
    const heard: Array<string | undefined> = []
    setTimeout(() => void first.release(), 700)
    await Effect.runPromise(
      Effect.scoped(
        Session.forReplay(options(SINGLE, 5_000), "replay B", (holder) => Effect.sync(() => void heard.push(holder)))
      ).pipe(Effect.provide(services()))
    )
    expect(heard).toHaveLength(2)
    expect(heard[0]).toContain("replay A")
    expect(heard[1]).toBeUndefined()
    await (await taken(SINGLE, "replay after")).release()
  })

  it("stops waiting the moment it is interrupted, holding nothing, on a clock that never moves", async () => {
    const first = await taken(SINGLE, "replay A")
    const heard: Array<string | undefined> = []
    await Effect.runPromise(
      Effect.gen(function*() {
        const waiting = yield* Effect.forkChild(
          Effect.scoped(
            Session.forReplay(options(SINGLE, 900_000), "replay B", (holder) =>
              Effect.sync(() => void heard.push(holder)))
          )
        )
        // The waiter has asked once and sleeps on the paused clock until it is told otherwise.
        while (heard.length === 0) yield* Effect.yieldNow
        yield* Fiber.interrupt(waiting)
      }).pipe(Effect.provide(Layer.merge(services(), TestClock.layer())))
    )
    expect(heard[0]).toContain("replay A")
    await first.release()
    // The interrupted waiter took nothing: the lane is free once its holder gives it back.
    await (await taken(SINGLE, "replay after")).release()
  })
})
