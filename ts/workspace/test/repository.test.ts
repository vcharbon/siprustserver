/**
 * The workspace's repository in a campaign run's provenance: its commit,
 * whether the files a run reads differ from it, and their content hash — the
 * same for the same files whatever commit, moved by none of the workspace's
 * other files; none where the workspace lies in no git work tree.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as Layout from "../src/layout.js"
import * as Repository from "../src/repository.js"
import { PACK } from "./pack.js"

let ws: string
beforeEach(() => {
  ws = fs.mkdtempSync(path.join(os.tmpdir(), "workspace-repository-"))
})
afterEach(() => fs.rmSync(ws, { recursive: true, force: true }))

const run = <A, E>(effect: Effect.Effect<A, E, NodeServices.NodeServices>) =>
  Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)))

const write = (file: string, text: string) => {
  fs.mkdirSync(path.dirname(path.join(ws, file)), { recursive: true })
  fs.writeFileSync(path.join(ws, file), text)
}

const git = (...args: Array<string>) =>
  execFileSync("git", ["-c", "user.name=Fakey McFakename", "-c", "user.email=fakey@example.com", ...args], {
    cwd: ws,
    encoding: "utf8"
  }).trim()

/** The paths a run of the tests' tooling reads: `tool.toml`, `campaigns/`, its `extras/`, the lanes. */
const READ = Repository.readPathsOf(PACK.configFile, [PACK.directories.extras])

const componentOf = (at: string = ws) => run(Effect.map(Repository.componentOf(READ, at), (one) => one?.component))

describe("the workspace's repository in a run's provenance", () => {
  it("is none where the workspace lies in no git work tree, or where no workspace is named", async () => {
    write("tool.toml", "")
    expect(await componentOf()).toBeUndefined()
    expect(await run(Repository.componentsOf(READ, Layout.layoutOf(PACK, ws)))).toEqual([])
    expect(await run(Repository.componentsOf(READ, undefined))).toEqual([])
  })

  it("states its commit, clean, and a content hash its other files never move", async () => {
    write("tool.toml", "")
    write("campaigns/a.ts", "export default 1\n")
    write("lanes/x.toml", "sip = \"backend\"\n")
    git("init", "-q")
    git("add", ".")
    git("commit", "-q", "-m", "one")
    const first = await componentOf()
    expect(first).toEqual({ name: "workspace", sha: git("rev-parse", "HEAD"), dirty: false, tree: expect.stringMatching(/^[0-9a-f]{64}$/) })
    write("runs/_e2e/r1/results.json", "{}\n")
    write("campaigns/node_modules/x/index.js", "1\n")
    expect(await componentOf()).toEqual(first)
    expect(await run(Repository.componentsOf(READ, Layout.layoutOf(PACK, ws)))).toEqual([first])
  })

  it("is dirty and hashes anew where a file of the tooling's own read directory changed", async () => {
    write("tool.toml", "")
    git("init", "-q")
    git("add", ".")
    git("commit", "-q", "-m", "one")
    const first = await componentOf()
    write("extras/shape.ts", "export default 1\n")
    const added = await componentOf()
    expect(added).toMatchObject({ sha: first!.sha, dirty: true })
    expect(added!.tree).not.toBe(first!.tree)
  })

  it("is dirty and hashes anew where a file a run reads changed, and hashes as before once it is back", async () => {
    write("tool.toml", "")
    write("campaigns/a.ts", "export default 1\n")
    git("init", "-q")
    git("add", ".")
    git("commit", "-q", "-m", "one")
    const first = await componentOf()
    write("campaigns/b.ts", "export default 2\n")
    const added = await componentOf()
    expect(added).toMatchObject({ sha: first!.sha, dirty: true })
    expect(added!.tree).not.toBe(first!.tree)
    git("add", ".")
    git("commit", "-q", "-m", "two")
    fs.rmSync(path.join(ws, "campaigns/b.ts"))
    git("commit", "-q", "-am", "three")
    expect(await componentOf()).toMatchObject({ dirty: false, tree: first!.tree })
  })

  it("reads a workspace that is a subdirectory of its repository under that subdirectory", async () => {
    write("proj/tool.toml", "")
    write("proj/campaigns/a.ts", "export default 1\n")
    write("other/x.txt", "x\n")
    git("init", "-q")
    git("add", ".")
    git("commit", "-q", "-m", "one")
    const inner = path.join(ws, "proj")
    write("other/y.txt", "y\n")
    write("proj/campaigns/node_modules/z.js", "1\n")
    expect(await componentOf(inner)).toMatchObject({ sha: git("rev-parse", "HEAD"), dirty: false })
    write("proj/campaigns/b.ts", "export default 2\n")
    expect(await componentOf(inner)).toMatchObject({ dirty: true })
  })

  it("says so where its .git names a git directory not visible here, its content hash still taken", async () => {
    write("tool.toml", "")
    write(".git", "gitdir: /nowhere/repo/.git/worktrees/ws\n")
    const one = await run(Repository.componentOf(READ, ws))
    expect(one).toEqual({
      component: { name: "workspace", sha: null, dirty: null, tree: expect.stringMatching(/^[0-9a-f]{64}$/) },
      note: Repository.NOT_VISIBLE
    })
  })
})
