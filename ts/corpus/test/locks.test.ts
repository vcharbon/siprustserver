/**
 * The box's readers-writer lock is what lets many replays load one host while a
 * real-clock lane can still have it to itself, so every rung of it is asserted
 * here: sharing, exclusion both ways, and the stale lock a killed sweep leaves
 * behind. The named singleton beside it is asserted the same way. Every case
 * takes its locks under a root of its own (`Roots.locks`).
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as FileSystemTag from "effect/FileSystem"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { Roots } from "../src/deployment/index.js"
import * as Locks from "../src/locks.js"

const NOW = "2026-08-27T00:00:00.000Z"
const NO_WAIT: Locks.Patience = { waitMs: 0 }

let root: string

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "lock-root-"))
})

afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

/** Roots whose lock root is `locks`, the rest unused. */
const rootsAt = (locks: string) =>
  Roots.layerWith({
    paths: () => Effect.die("unused"),
    pcapRoots: Effect.succeed([]),
    locks: Effect.succeed(locks),
    bench: Effect.succeed(locks)
  })

/** One take, under `locks` (the case's root where absent). */
const take = (
  effect: Effect.Effect<Locks.Taken | Locks.Held, Roots.RootsUnstated, Roots.Service | FileSystem.FileSystem>,
  locks = root
) =>
  Effect.runPromise(effect.pipe(Effect.provide(Layer.merge(rootsAt(locks), NodeServices.layer))))


/** The box's lock directory under the case's own lock root. */
const box = (): string => path.join(root, "box.lock.d")

/** A holder file written for a pid nobody is running, which is what a kill leaves. */
const plant = (file: string, pid: number): void => {
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, `${JSON.stringify({ pid, since: NOW, what: "a killed sweep" })}\n`)
}

const held = (lock: Locks.Taken | Locks.Held): string => {
  expect(lock).toHaveProperty("held")
  return (lock as Locks.Held).held
}

const release = (lock: Locks.Taken | Locks.Held): Promise<void> => {
  expect(Locks.isTaken(lock)).toBe(true)
  return Effect.runPromise((lock as Locks.Taken).release)
}

describe("the box", () => {
  it("gives its quiet to one real-clock replay at a time", async () => {
    const first = await take(Locks.quiet("replay real A", NO_WAIT))
    expect(held(await take(Locks.quiet("replay real B", NO_WAIT)))).toContain("replay real A")
    await release(first)
    await release(await take(Locks.quiet("replay real B", NO_WAIT)))
  })

  it("takes every load-bearing replay at once", async () => {
    const a = await take(Locks.load("replay one", NO_WAIT))
    const b = await take(Locks.load("replay two", NO_WAIT))
    await release(a)
    await release(b)
  })

  it("refuses the quiet while a replay is loading it, and names it", async () => {
    const reader = await take(Locks.load("replay paused", NO_WAIT))
    expect(held(await take(Locks.quiet("replay real", NO_WAIT)))).toContain("replay paused")
    // The refused replay left the marker behind it would have blocked readers with.
    await release(await take(Locks.load("replay two", NO_WAIT)))
    await release(reader)
    await release(await take(Locks.quiet("replay real", NO_WAIT)))
  })

  it("refuses a load-bearing replay while the quiet is held, and names it", async () => {
    const writer = await take(Locks.quiet("replay real", NO_WAIT))
    expect(held(await take(Locks.load("replay paused", NO_WAIT)))).toContain("replay real")
    await release(writer)
    await release(await take(Locks.load("replay paused", NO_WAIT)))
  })

  it("breaks the quiet a killed replay left behind", async () => {
    // A pid this process cannot signal reads as alive, so the dead holder has
    // to be one nobody is running.
    plant(path.join(box(), "writer.json"), 0x7fff_fffe)
    await release(await take(Locks.quiet("replay real", NO_WAIT)))
  })

  it("breaks the reader a killed replay left behind", async () => {
    plant(path.join(box(), "readers", "stale.json"), 0x7fff_fffe)
    await release(await take(Locks.quiet("replay real", NO_WAIT)))
  })

  it("tells the watcher whom it waits for, then that it waits no more", async () => {
    const writer = await take(Locks.quiet("replay real", NO_WAIT))
    const heard: Array<string | undefined> = []
    const watch: Locks.Watch = (who) => Effect.sync(() => void heard.push(who))
    const waiting = take(Locks.load("replay paused", { waitMs: 5_000 }, watch))
    await new Promise((resolve) => setTimeout(resolve, 200))
    await release(writer)
    await release(await waiting)
    expect(heard[0]).toContain("replay real")
    expect(heard.at(-1)).toBeUndefined()
  })
})

describe("the lock root", () => {
  it("is the one Roots.locks names: a holder under it refuses by name, a holder elsewhere is no one", async () => {
    const campaign = fs.mkdtempSync(path.join(os.tmpdir(), "locks-campaign-"))
    const mine = fs.mkdtempSync(path.join(os.tmpdir(), "locks-mine-"))
    try {
      // A live process holding the quiet of its root, as a campaign on the box does.
      fs.mkdirSync(path.join(campaign, "box.lock.d"))
      fs.writeFileSync(
        path.join(campaign, "box.lock.d", "writer.json"),
        `${JSON.stringify({ pid: process.pid, since: NOW, what: "a live campaign" })}\n`
      )
      expect(held(await take(Locks.quiet("replay real", NO_WAIT), campaign))).toContain("a live campaign")
      const quiet = await take(Locks.quiet("replay real", NO_WAIT), mine)
      expect(fs.existsSync(path.join(mine, "box.lock.d", "writer.json"))).toBe(true)
      await release(quiet)
      const lane = await take(Locks.exclusive("lane-x", "lane run", NO_WAIT), mine)
      expect(fs.existsSync(path.join(mine, "lane-x.json"))).toBe(true)
      await release(lane)
    } finally {
      fs.rmSync(campaign, { recursive: true, force: true })
      fs.rmSync(mine, { recursive: true, force: true })
    }
  })
})

describe("a box-wide singleton", () => {
  it("goes to one caller at a time, by name", async () => {
    const name = `test-${process.pid}`
    const first = await take(Locks.exclusive(name, "lane run A", NO_WAIT))
    expect(held(await take(Locks.exclusive(name, "lane run B", NO_WAIT)))).toContain("lane run A")
    await release(await take(Locks.exclusive(`${name}-other`, "another lane", NO_WAIT)))
    await release(first)
    await release(await take(Locks.exclusive(name, "lane run B", NO_WAIT)))
  })

  /**
   * A file system whose exclusive create lands the file empty and its content
   * a moment later, as an open-then-write does: a holder's file read in that
   * window must never read as a stale lock to break.
   */
  const slowExclusive = Layer.effect(
    FileSystemTag.FileSystem,
    Effect.map(FileSystemTag.FileSystem, (real) => ({
      ...real,
      writeFileString: (file: string, data: string, options?: Parameters<FileSystem.FileSystem["writeFileString"]>[2]) =>
        options?.flag === "wx"
          ? real.writeFileString(file, "", options).pipe(
            Effect.andThen(Effect.sleep("150 millis")),
            Effect.andThen(real.writeFileString(file, data))
          )
          : real.writeFileString(file, data, options)
    }))
  ).pipe(Layer.provide(NodeServices.layer))

  it("goes to one of two callers racing for it, a holder still writing its file included", async () => {
    const name = `race-${process.pid}`
    const one = Locks.exclusive(name, "lane run A", NO_WAIT)
    const two = Effect.sleep("40 millis").pipe(Effect.andThen(Locks.exclusive(name, "lane run B", NO_WAIT)))
    const both = await Effect.runPromise(
      Effect.all([one, two], { concurrency: "unbounded" }).pipe(
        Effect.provide(Layer.merge(rootsAt(root), slowExclusive))
      )
    )
    expect(both.filter(Locks.isTaken)).toHaveLength(1)
  })
})
