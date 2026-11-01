/**
 * A checkout's code where git cannot say: a directory that is no repository,
 * and a host with no git on `PATH`. Either reads as unknown, never as an
 * error, so a run on such a host is recorded with its code unknown.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import { Git } from "../src/git.js"

const dirs: Array<string> = []
const savedPath = process.env["PATH"]

afterEach(() => {
  process.env["PATH"] = savedPath
  for (const d of dirs.splice(0)) fs.rmSync(d, { recursive: true, force: true })
})

const scratch = (): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "git-unknown-"))
  dirs.push(dir)
  return dir
}

const read = (dir: string) =>
  Effect.runPromise(
    Effect.all([Git.componentOf("code", dir), Git.branchOf(dir)]).pipe(Effect.provide(NodeServices.layer))
  )

describe("a checkout git cannot read", () => {
  it("is unknown where the directory is no repository", async () => {
    const [component, branch] = await read(scratch())
    expect(component).toEqual({ name: "code", sha: null, dirty: null, tree: null })
    expect(branch).toBeNull()
  })

  it("is unknown where no git is on PATH", async () => {
    const dir = scratch()
    process.env["PATH"] = dir
    const [component, branch] = await read(process.cwd())
    expect(component).toEqual({ name: "code", sha: null, dirty: null, tree: null })
    expect(branch).toBeNull()
  })
})
