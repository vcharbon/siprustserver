/**
 * A host tool is found only as an executable regular file in a directory of
 * `PATH`; a feature lacking one says so in the shared words, naming the tool.
 */
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { isExecutable, onPath, unavailable } from "../src/host-tools.js"

const home = fs.mkdtempSync(path.join(os.tmpdir(), "host-tools-"))
afterAll(() => fs.rmSync(home, { recursive: true, force: true }))

const run = <A>(effect: Effect.Effect<A>): Promise<A> => Effect.runPromise(effect)

const dirWith = (name: string, files: Readonly<Record<string, number>>): string => {
  const dir = path.join(home, name)
  fs.mkdirSync(dir)
  for (const [file, mode] of Object.entries(files)) fs.writeFileSync(path.join(dir, file), "#!/bin/sh\n", { mode })
  return dir
}

describe("a host tool", () => {
  it("is executable only as a regular file with the execute bit", async () => {
    const dir = dirWith("modes", { run: 0o755, plain: 0o644 })
    fs.mkdirSync(path.join(dir, "folder"))
    expect(await run(isExecutable(path.join(dir, "run")))).toBe(true)
    expect(await run(isExecutable(path.join(dir, "plain")))).toBe(false)
    expect(await run(isExecutable(path.join(dir, "folder")))).toBe(false)
    expect(await run(isExecutable(path.join(dir, "absent")))).toBe(false)
  })

  it("is on PATH where some directory of it holds the executable, never as a non-executable file", async () => {
    const plain = dirWith("plain-path", { tool: 0o644 })
    const real = dirWith("real-path", { tool: 0o755 })
    expect(await run(onPath("tool", { PATH: [plain, real].join(path.delimiter) }))).toBe(true)
    expect(await run(onPath("tool", { PATH: plain }))).toBe(false)
    expect(await run(onPath("tool", { PATH: "" }))).toBe(false)
    expect(await run(onPath("tool", {}))).toBe(false)
  })

  it("lacking, makes its feature say it is not available, naming the tool", () => {
    expect(unavailable("the build", "cargo")).toBe("the build: not available in this install (no cargo on PATH)")
  })
})
