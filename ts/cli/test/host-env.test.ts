/**
 * The host's env files as defaults: a key already exported is kept, a key the
 * facts do not list is ignored and said, the first file to state a key wins,
 * a path key resolves against its file's directory, and a file the host lacks
 * is skipped.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as HostEnv from "../src/host-env.js"

let dir: string
beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "host-env-"))
})
afterEach(() => {
  fs.rmSync(dir, { recursive: true, force: true })
})

const host = (files: ReadonlyArray<string>) => ({
  files,
  keys: { STORE_URL: "the store", ROOT: "a root", EXPORTED: "already set" },
  pathKeys: new Set(["ROOT"])
})

describe("HostEnv.parse", () => {
  it("reads KEY=VALUE lines, skipping comments and blanks, unquoting, the first statement winning", () => {
    const read = HostEnv.parse("# a comment\n\nexport A=1\nB='two words'\nA=again\nnot a line\n")
    expect([...read]).toEqual([["A", "1"], ["B", "two words"]])
  })
})

describe("HostEnv.apply", () => {
  it("applies the listed keys as defaults, resolving a path key against its file", async () => {
    const first = path.join(dir, "first.env")
    const second = path.join(dir, "second.env")
    fs.writeFileSync(first, "STORE_URL=postgres://one\nROOT=../data\nUNLISTED=x\nEXPORTED=file\n")
    fs.writeFileSync(second, "STORE_URL=postgres://two\n")
    const env: NodeJS.ProcessEnv = { EXPORTED: "shell" }
    const applied = await Effect.runPromise(
      HostEnv.apply(host([first, second, path.join(dir, "absent.env")]), env).pipe(Effect.provide(NodeServices.layer))
    )
    expect(env).toEqual({ EXPORTED: "shell", STORE_URL: "postgres://one", ROOT: path.resolve(dir, "../data") })
    expect(applied).toEqual([
      { file: first, set: ["STORE_URL", "ROOT"], ignored: ["UNLISTED", "EXPORTED"] },
      { file: second, set: [], ignored: ["STORE_URL"] }
    ])
  })

  it("applies the deployment's own defaults first, so they win over a file and never over an exported key", async () => {
    const file = path.join(dir, "host.env")
    fs.writeFileSync(file, "STORE_URL=postgres://file\nROOT=from-file\n")
    const env: NodeJS.ProcessEnv = { EXPORTED: "shell" }
    const before = (target: NodeJS.ProcessEnv) =>
      Effect.sync(() => {
        target.STORE_URL = "postgres://own"
        return [{ file: "own.conf", set: ["STORE_URL"], ignored: [] }]
      })
    const applied = await Effect.runPromise(
      HostEnv.apply({ ...host([file]), before }, env).pipe(Effect.provide(NodeServices.layer))
    )
    expect(env).toEqual({ EXPORTED: "shell", STORE_URL: "postgres://own", ROOT: path.resolve(dir, "from-file") })
    expect(applied).toEqual([
      { file: "own.conf", set: ["STORE_URL"], ignored: [] },
      { file, set: ["ROOT"], ignored: ["STORE_URL"] }
    ])
  })

  it("fails with the deployment's refusal and applies no file", async () => {
    const file = path.join(dir, "host.env")
    fs.writeFileSync(file, "STORE_URL=postgres://file\n")
    const env: NodeJS.ProcessEnv = {}
    const before = () => Effect.fail(new HostEnv.Refused({ reason: "own.conf: port: not a port" }))
    const exit = await Effect.runPromiseExit(
      HostEnv.apply({ ...host([file]), before }, env).pipe(Effect.provide(NodeServices.layer))
    )
    expect(exit._tag).toBe("Failure")
    expect(String(exit)).toContain("own.conf: port: not a port")
    expect(env).toEqual({})
  })
})
