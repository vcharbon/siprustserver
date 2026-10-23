/**
 * A detached command: its launch returns while it runs, its record names the
 * supervisor by pid and start ticks, its output lands in the log and its exit
 * code in `exit`; a TERM to the supervisor's group is forwarded to the
 * command and its exit still recorded; a record still running refuses a
 * second launch over it.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as Detached from "../src/detached.js"
import * as Liveness from "../src/liveness.js"

const run = <A, E>(effect: Effect.Effect<A, E, Detached.Requirements>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

const roots: Array<string> = []
const dirOf = (): string => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "detached-"))
  roots.push(root)
  return path.join(root, "job")
}

afterEach(() => {
  for (const root of roots.splice(0)) fs.rmSync(root, { recursive: true, force: true })
})

/** Polls until `dir` holds an exit code. */
const exited = async (dir: string): Promise<number> => {
  for (let at = 0; at < 200; at++) {
    const code = await run(Detached.exitOf(dir))
    if (code !== undefined) return code
    await new Promise((resolve) => setTimeout(resolve, 25))
  }
  throw new Error(`${dir} never recorded an exit`)
}

/** Polls until `dir`'s log holds `line`: the command has reached it. */
const said = async (dir: string, line: string): Promise<void> => {
  for (let at = 0; at < 200; at++) {
    if (fs.readFileSync(path.join(dir, "log"), "utf8").includes(line)) return
    await new Promise((resolve) => setTimeout(resolve, 25))
  }
  throw new Error(`${dir} never said ${line}`)
}

describe("Detached.launch", () => {
  it("records the command, its output and its exit, the supervisor by pid and ticks", async () => {
    const dir = dirOf()
    const launched = await run(Detached.launch(dir, ["sh", "-c", "echo said; echo 'on err' >&2; exit 3"]))
    expect(await exited(dir)).toBe(3)
    expect(fs.readFileSync(launched.log, "utf8")).toBe("said\non err\n")
    expect(fs.readFileSync(path.join(dir, "cmd"), "utf8")).toBe("sh -c 'echo said; echo '\\''on err'\\'' >&2; exit 3'\n")
    const record = await run(Liveness.recorded(path.join(dir, "pid")))
    expect(record?.pid).toBe(launched.pid)
    expect(await run(Detached.running(dir))).toBe(false)
  })

  it("runs in the directory and environment it is given", async () => {
    const dir = dirOf()
    const cwd = path.dirname(dir)
    await run(Detached.launch(dir, ["sh", "-c", "pwd; echo $DETACHED_FACT"], { cwd, env: { DETACHED_FACT: "stated" } }))
    expect(await exited(dir)).toBe(0)
    expect(fs.readFileSync(path.join(dir, "log"), "utf8")).toBe(`${fs.realpathSync(cwd)}\nstated\n`)
  })

  it("takes a TERM to its group in the command and records the command's own exit; refuses a launch over a running record", async () => {
    const dir = dirOf()
    const launched = await run(Detached.launch(dir, ["sh", "-c", "trap 'exit 7' TERM; echo ready; sleep 30 & wait"]))
    await said(dir, "ready")
    expect(await run(Detached.running(dir))).toBe(true)
    const again = await Effect.runPromiseExit(
      Detached.launch(dir, ["true"]).pipe(Effect.provide(NodeServices.layer))
    )
    expect(again._tag).toBe("Failure")
    await run(Detached.terminateGroup(launched.pid))
    expect(await exited(dir)).toBe(7)
  })
})
