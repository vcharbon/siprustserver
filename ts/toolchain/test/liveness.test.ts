/**
 * Liveness off a pid record: this process runs, a record with other start
 * ticks is another process, an ended process not yet reaped runs no more, an
 * unreadable or malformed record is none.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as child_process from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import * as Liveness from "../src/liveness.js"

const run = <A>(effect: Effect.Effect<A, never, FileSystem.FileSystem>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

/** Polls until `pid` is a zombie; its `/proc` stat fields from field 3 on. */
const zombie = async (pid: number): Promise<Array<string>> => {
  for (let at = 0; at < 200; at++) {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8")
    const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ")
    if (fields[0] === "Z") return fields
    await new Promise((resolve) => setTimeout(resolve, 25))
  }
  throw new Error(`${pid} never became a zombie`)
}

describe("Liveness", () => {
  it("reads this process as running by pid and by its start ticks", async () => {
    const ticks = await run(Liveness.ticksOf(process.pid))
    expect(ticks).toMatch(/^\d+$/)
    expect(await run(Liveness.recordedAlive({ pid: process.pid, ticks }))).toBe(true)
    expect(await run(Liveness.recordedAlive({ pid: process.pid, ticks: "1" }))).toBe(false)
    expect(await run(Liveness.alive(process.pid))).toBe(true)
  })

  it("reads an ended process its parent has not reaped as gone", async () => {
    // The inner shell ends at once; its parent, exec'd into sleep, never reaps it.
    const parent = child_process.spawn("sh", ["-c", "sh -c 'exit 0' & echo $!; exec sleep 30"], {
      stdio: ["ignore", "pipe", "ignore"]
    })
    try {
      const pid = Number(await new Promise<string>((resolve) => parent.stdout.once("data", (chunk) => resolve(String(chunk)))))
      const fields = await zombie(pid)
      expect(await run(Liveness.ticksOf(pid))).toBeUndefined()
      expect(await run(Liveness.recordedAlive({ pid, ticks: fields[22 - 3] }))).toBe(false)
    } finally {
      parent.kill("SIGKILL")
    }
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
