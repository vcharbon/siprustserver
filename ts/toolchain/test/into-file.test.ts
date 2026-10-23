/**
 * A command whose ANSWER is a file: the parent never holds what it wrote.
 *
 * A flows document of a few thousand calls is hundreds of megabytes, so a
 * parent that collected the command's stdout would have to cap it — and that
 * cap is the first thing a big capture meets.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { IntoFile } from "../src/into-file.js"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "into-file-test-"))

afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

const dirOf = (name: string): string => {
  const dir = path.join(scratch, name)
  fs.mkdirSync(dir, { recursive: true })
  return dir
}

/** What `dir` holds, whole, whatever the writer named it. */
const written = (dir: string): string =>
  fs.readdirSync(dir).map((entry) => fs.readFileSync(path.join(dir, entry), "utf8")).join("")

const until = async (holds: () => boolean, ms: number): Promise<boolean> => {
  for (let waited = 0; waited < ms; waited += 10) {
    if (holds()) return true
    await new Promise((resume) => setTimeout(resume, 10))
  }
  return holds()
}

const into = (binary: string, args: ReadonlyArray<string>, target: string) =>
  Effect.runPromise(IntoFile.run(binary, args, target).pipe(Effect.provide(NodeServices.layer)))

describe("IntoFile.run", () => {
  it("writes the command's output as it arrives, holding none of it", async () => {
    const dir = dirOf("streaming")
    const target = path.join(dir, "out")
    const done = into("sh", ["-c", "printf hello; sleep 2"], target)
    // The bytes are on disk while the command still runs, so nothing collected them.
    expect(await until(() => written(dir) === "hello", 1500)).toBe(true)
    expect(await done).toBeUndefined()
    expect(fs.readFileSync(target, "utf8")).toBe("hello")
  })

  it("leaves no file behind a command that failed, and says what it said", async () => {
    const dir = dirOf("failed")
    const target = path.join(dir, "out")
    const why = await into("sh", ["-c", "echo half; echo boom >&2; exit 3"], target)
    expect(why).toContain("boom")
    expect(why).toContain("exited 3")
    expect(fs.readdirSync(dir)).toEqual([])
  })

  it("names the signal a command was killed by", async () => {
    const dir = dirOf("killed")
    const why = await into("sh", ["-c", "echo dying >&2; kill -KILL $$"], path.join(dir, "out"))
    expect(why).toContain("killed by SIGKILL")
    expect(fs.readdirSync(dir)).toEqual([])
  })

  it("answers, rather than failing, when the target cannot be written at all", async () => {
    // A caller that runs many of these must lose the one capture, not the batch.
    const dir = dirOf("unwritable")
    fs.chmodSync(dir, 0o500)
    try {
      const why = await into("sh", ["-c", "echo hi"], path.join(dir, "under", "out"))
      expect(why).toBeDefined()
    } finally {
      fs.chmodSync(dir, 0o700)
    }
  })

  it("says so when the command does not exist", async () => {
    const dir = dirOf("missing")
    const target = path.join(dir, "out")
    const why = await into(path.join(scratch, "no-such-binary"), [], target)
    expect(why).toBeDefined()
    expect(fs.readdirSync(dir)).toEqual([])
  })
})

describe("IntoFile.exec", () => {
  it("keeps the head of what the command said, and its code", async () => {
    const dir = dirOf("exec")
    const ended = await Effect.runPromise(
      IntoFile.exec("sh", ["-c", "printf 'a\"b\\nc' >&2; exit 2"], path.join(dir, "out"), { stderrHead: 600 }).pipe(
        Effect.provide(NodeServices.layer)
      )
    )
    expect(ended).toEqual({ broke: undefined, exit: 2, signal: null, stderr: "a\"b\nc" })
  })
})
