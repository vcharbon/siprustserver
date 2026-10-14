/**
 * A hook is told the job's captures in a way the kernel accepts: the list in
 * `HOOK_CAPTURE` while it fits one environment string, its count there and
 * the file `HOOK_CAPTURES_FILE` otherwise (the file always). A hook the
 * kernel refuses to spawn is a log with exit -1, never a failed job.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as Hooks from "../../src/jobs/hooks.js"

let dir: string

beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "hooks-test-"))
})
afterEach(() => {
  fs.rmSync(dir, { recursive: true, force: true })
})

const env = (captures: ReadonlyArray<string>): Hooks.HookEnv => ({
  job: 7,
  jobDir: dir,
  captures,
  lane: "paused",
  startedAt: "2026-09-21T00:00:00.000Z"
})

const run = <A, E>(effect: Effect.Effect<A, E, Hooks.Requirements>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

const tell: Hooks.Hook = {
  name: "tell",
  when: "after",
  command: "printf '%s\\n' \"$HOOK_CAPTURE\"; wc -l < \"$HOOK_CAPTURES_FILE\""
}

describe("Hooks.run", () => {
  it("hands a small list in HOOK_CAPTURE and every capture in the file", async () => {
    const logs = await run(Hooks.run([tell], "after", env(["a.pcap", "b.pcap"]), dir, dir))
    expect(logs).toHaveLength(1)
    expect(logs[0].exit).toBe(0)
    expect(logs[0].path).toBe(path.join(dir, "tell.log"))
    expect(fs.readFileSync(path.join(dir, "captures.txt"), "utf8")).toBe("a.pcap\nb.pcap\n")
    expect(fs.readFileSync(path.join(dir, "tell.log"), "utf8")).toBe("exit 0\na.pcap b.pcap\n2\n")
  })

  it("states the count in HOOK_CAPTURE when the list would not fit one environment string", async () => {
    const captures = Array.from({ length: 12_000 }, (_, i) => `capture_${String(i).padStart(36, "0")}.pcap.gz`)
    expect(captures.join(" ").length).toBeGreaterThan(Hooks.CAPTURE_LIMIT)
    const logs = await run(Hooks.run([tell], "after", env(captures), dir, dir))
    expect(logs[0].exit).toBe(0)
    expect(fs.readFileSync(logs[0].path, "utf8")).toBe("exit 0\n12000 capture(s), see HOOK_CAPTURES_FILE\n12000\n")
  })

  it("keeps every byte a command wrote, streamed to its log and nothing left beside it", async () => {
    // 3 MiB: far past what any in-memory tail would keep of a busy log.
    const loud: Hooks.Hook = { name: "loud", when: "after", command: "head -c 3145728 /dev/zero | tr '\\0' x; echo; echo end" }
    const logs = await run(Hooks.run([loud], "after", env(["a.pcap"]), dir, dir))
    const text = fs.readFileSync(logs[0].path, "utf8")
    expect(text.length).toBe("exit 0\n".length + 3_145_728 + "\nend\n".length)
    expect(text.startsWith("exit 0\nxxx")).toBe(true)
    expect(text.endsWith("x\nend\n")).toBe(true)
    expect(fs.readdirSync(dir).sort()).toEqual(["captures.txt", "loud.log"])
  })

  it("records a hook the kernel refuses to spawn as its log, exit -1", async () => {
    const tooLong: Hooks.Hook = { name: "huge", when: "before", command: `: ${"x".repeat(200_000)}` }
    const logs = await run(Hooks.run([tooLong], "before", env(["a.pcap"]), dir, dir))
    expect(logs[0].exit).toBe(-1)
    const text = fs.readFileSync(path.join(dir, "huge.log"), "utf8")
    expect(text.startsWith("exit -1\n")).toBe(true)
    expect(text).toContain("spawn failed: spawn E2BIG")
  })
})
