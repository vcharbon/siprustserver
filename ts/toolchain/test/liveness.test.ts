/**
 * Liveness off a pid record: this process runs, a record with other start
 * ticks is another process, an unreadable or malformed record is none.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import * as Liveness from "../src/liveness.js"

const run = <A>(effect: Effect.Effect<A, never, FileSystem.FileSystem>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

describe("Liveness", () => {
  it("reads this process as running by pid and by its start ticks", async () => {
    const ticks = await run(Liveness.ticksOf(process.pid))
    expect(ticks).toMatch(/^\d+$/)
    expect(await run(Liveness.recordedAlive({ pid: process.pid, ticks }))).toBe(true)
    expect(await run(Liveness.recordedAlive({ pid: process.pid, ticks: "1" }))).toBe(false)
    expect(await run(Liveness.alive(process.pid))).toBe(true)
  })

  it("reads a record's pid and ticks, and none off a missing or malformed file", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "liveness-"))
    try {
      fs.writeFileSync(path.join(dir, "pid"), "42 9001\n")
      fs.writeFileSync(path.join(dir, "bare"), "42\n")
      fs.writeFileSync(path.join(dir, "bad"), "nope\n")
      expect(await run(Liveness.recorded(path.join(dir, "pid")))).toEqual({ pid: 42, ticks: "9001" })
      expect(await run(Liveness.recorded(path.join(dir, "bare")))).toEqual({ pid: 42, ticks: undefined })
      expect(await run(Liveness.recorded(path.join(dir, "bad")))).toBeUndefined()
      expect(await run(Liveness.recorded(path.join(dir, "absent")))).toBeUndefined()
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })
})
